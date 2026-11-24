#import <AppKit/AppKit.h>
#import <Security/Security.h>
#import <LocalAuthentication/LocalAuthentication.h>
#include <stdint.h>

// AppKit window access is confined to the Slint main thread.
float mc_pointer_y(void) {
    NSView *view = NSApp.keyWindow.contentView;
    if (!view) return -1;
    NSPoint p = [view convertPoint:view.window.mouseLocationOutsideOfEventStream fromView:nil];
    if (!NSPointInRect(p, view.bounds)) return -1;
    return view.isFlipped ? p.y - NSMinY(view.bounds) : NSMaxY(view.bounds) - p.y;
}

bool mc_foreground(void) {
    return NSApp.isActive && NSApp.keyWindow.isMainWindow;
}

bool mc_dark(void) {
    return [[NSApp.effectiveAppearance bestMatchFromAppearancesWithNames:
        @[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]] isEqualToString:NSAppearanceNameDarkAqua];
}
bool mc_motion(void) { return !NSWorkspace.sharedWorkspace.accessibilityDisplayShouldReduceMotion; }
bool mc_glass(void) {
    return !NSWorkspace.sharedWorkspace.accessibilityDisplayShouldReduceTransparency &&
           !NSWorkspace.sharedWorkspace.accessibilityDisplayShouldIncreaseContrast;
}

uint32_t mc_clip_sequence(void) { return (uint32_t)NSPasteboard.generalPasteboard.changeCount; }

int mc_clip_read(uint8_t *dst, size_t limit, uint32_t *sequence) {
    @autoreleasepool {
        NSPasteboard *board = NSPasteboard.generalPasteboard;
        *sequence = (uint32_t)board.changeCount;
        NSString *value = [board stringForType:NSPasteboardTypeString];
        if (!value) return -1;
        NSUInteger size = [value lengthOfBytesUsingEncoding:NSUTF8StringEncoding];
        if (size > limit) return -2;
        NSUInteger used = 0;
        if (size && ![value getBytes:dst maxLength:limit usedLength:&used encoding:NSUTF8StringEncoding
                         options:0 range:NSMakeRange(0, value.length) remainingRange:NULL]) return -2;
        return *sequence == (uint32_t)board.changeCount ? (int)used : -1;
    }
}

int mc_clip_write(const uint8_t *src, size_t size, int64_t expected, uint32_t *sequence) {
    @autoreleasepool {
        NSString *value = [[NSString alloc] initWithBytes:src length:size encoding:NSUTF8StringEncoding];
        if (!value) return -1;
        NSPasteboard *board = NSPasteboard.generalPasteboard;
        if (expected >= 0 && (uint32_t)expected != (uint32_t)board.changeCount) return 0;
        [board clearContents];
        if (![board setString:value forType:NSPasteboardTypeString]) return -1;
        *sequence = (uint32_t)board.changeCount;
        return 1;
    }
}

int mc_file_dialog(const char *name, char *dst, size_t limit) {
    @autoreleasepool {
        NSSavePanel *panel = name ? [NSSavePanel savePanel] : [NSOpenPanel openPanel];
        if (name) panel.nameFieldStringValue = [NSString stringWithUTF8String:name];
        if ([panel runModal] != NSModalResponseOK) return 0;
        return [panel.URL getFileSystemRepresentation:dst maxLength:limit] ? 1 : -1;
    }
}

static NSMutableDictionary *keychainQuery(const char *account) {
    return [@{(__bridge id)kSecClass:(__bridge id)kSecClassGenericPassword,
              (__bridge id)kSecAttrService:@"MacCanvas.profile",
              (__bridge id)kSecAttrAccount:[NSString stringWithUTF8String:account]} mutableCopy];
}

int32_t mc_vault_load(const char *account, uint8_t *dst, size_t maximum, size_t *size) {
    @autoreleasepool {
        NSMutableDictionary *query = keychainQuery(account);
        query[(__bridge id)kSecReturnData] = @YES;
        CFTypeRef result = NULL;
        OSStatus status = SecItemCopyMatching((__bridge CFDictionaryRef)query, &result);
        if (status) return status;
        NSData *data = CFBridgingRelease(result);
        if (data.length > maximum) return errSecDecode;
        [data getBytes:dst length:data.length];
        *size = data.length;
        return errSecSuccess;
    }
}

int32_t mc_vault_save(const char *account, const uint8_t *src, size_t size) {
    @autoreleasepool {
        NSMutableDictionary *query = keychainQuery(account);
        NSDictionary *values = @{(__bridge id)kSecValueData:[NSData dataWithBytes:src length:size]};
        OSStatus status = SecItemUpdate((__bridge CFDictionaryRef)query, (__bridge CFDictionaryRef)values);
        if (status != errSecItemNotFound) return status;
        [query addEntriesFromDictionary:values];
        return SecItemAdd((__bridge CFDictionaryRef)query, NULL);
    }
}

int32_t mc_vault_delete(const char *account) {
    @autoreleasepool {
        OSStatus status = SecItemDelete((__bridge CFDictionaryRef)keychainQuery(account));
        return status == errSecItemNotFound ? errSecSuccess : status;
    }
}

bool mc_consent(void) {
    @autoreleasepool {
        LAContext *context = [LAContext new];
        if (![context canEvaluatePolicy:LAPolicyDeviceOwnerAuthentication error:nil]) return false;
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        __block BOOL approved = NO;
        [context evaluatePolicy:LAPolicyDeviceOwnerAuthentication
                localizedReason:@"Connect with your saved MacCanvas login"
                          reply:^(BOOL success, NSError *error) {
            (void)error;
            approved = success;
            dispatch_semaphore_signal(done);
        }];
        dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
        return approved;
    }
}
