#include <AudioToolbox/AudioToolbox.h>
#include <stdbool.h>
#include <stdint.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>

static AudioStreamBasicDescription pcm_format(bool floating) {
    UInt32 bytes = floating ? 4 : 2;
    return (AudioStreamBasicDescription){
        .mSampleRate = 48000, .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsPacked | (floating ? kAudioFormatFlagIsFloat : kAudioFormatFlagIsSignedInteger),
        .mBytesPerPacket = 2 * bytes, .mFramesPerPacket = 1,
        .mBytesPerFrame = 2 * bytes, .mChannelsPerFrame = 2, .mBitsPerChannel = 8 * bytes
    };
}

OSStatus mc_audio_decoder(AudioConverterRef *out) {
    AudioStreamBasicDescription input = {
        .mSampleRate = 48000, .mFormatID = kAudioFormatMPEG4AAC_ELD_SBR,
        .mFramesPerPacket = 480, .mChannelsPerFrame = 2
    };
    AudioStreamBasicDescription output = pcm_format(false);
    OSStatus status = AudioConverterNew(&input, &output, out);
    if (status) return status;
    // AudioToolbox expects the MPEG-4 ES descriptor, including its nested
    // DecoderSpecificInfo. Apple screen sharing sends the six-byte ASC alone.
    const uint8_t cookie[] = {
        0x03, 29, 0, 1, 0,                // ES_Descriptor
        0x04, 21, 0x40, 0x15, 0, 0, 0,   // AAC audio DecoderConfigDescriptor
        0, 0, 0, 0, 0, 0, 0, 0,          // variable bitrate
        0x05, 6, 0xf8, 0xe6, 0x51, 0x32, 0xe0, 0x00,
        0x06, 1, 2                       // SLConfigDescriptor
    };
    status = AudioConverterSetProperty(*out, kAudioConverterDecompressionMagicCookie, sizeof(cookie), cookie);
    if (status) { AudioConverterDispose(*out); *out = NULL; }
    return status;
}

typedef struct {
    const uint8_t *data;
    AudioStreamPacketDescription desc;
} InputPacket;

// Signal temporary starvation without marking this continuous RTP stream as finished.
enum { NeedNextPacket = 0x6d636e70 };

static OSStatus take_packet(AudioConverterRef converter, UInt32 *packets, AudioBufferList *out,
                            AudioStreamPacketDescription **description, void *context) {
    (void)converter;
    InputPacket *input = context;
    if (!input->data) { *packets = 0; return NeedNextPacket; }
    *packets = 1;
    out->mNumberBuffers = 1;
    out->mBuffers[0] = (AudioBuffer){2, input->desc.mDataByteSize, (void *)input->data};
    if (description) *description = &input->desc;
    input->data = NULL;
    return noErr;
}

OSStatus mc_audio_decode(AudioConverterRef converter, const uint8_t *bytes, UInt32 len,
                         int16_t *stereo, UInt32 *frames) {
    InputPacket input = {bytes, {0, 480, len}};
    // Request room for several packets so the converter always asks for more
    // input before returning. That callback releases the borrowed packet data.
    AudioBufferList out = {1, {{2, *frames * 4, stereo}}};
    OSStatus status = AudioConverterFillComplexBuffer(converter, take_packet, &input, frames, &out, NULL);
    if (status == NeedNextPacket) return noErr;
    AudioConverterReset(converter);
    return status ? status : kAudioConverterErr_InvalidOutputSize;
}

typedef struct {
    AudioQueueRef queue;
    void *context;
    bool (*fill)(void *, uint8_t *, uint32_t);
    _Atomic OSStatus error;
} Output;

static void refill(void *context, AudioQueueRef queue, AudioQueueBufferRef buffer) {
    Output *output = context;
    const UInt32 frames = buffer->mAudioDataBytesCapacity / 8;
    if (!output->fill(output->context, buffer->mAudioData, frames)) return;
    buffer->mAudioDataByteSize = frames * 8;
    OSStatus status = AudioQueueEnqueueBuffer(queue, buffer, 0, NULL);
    if (status) output->error = status;
}

OSStatus mc_audio_output(void *context, bool (*fill)(void *, uint8_t *, uint32_t), Output **out) {
    Output *output = calloc(1, sizeof(*output));
    if (!output) return ENOMEM;
    output->context = context;
    output->fill = fill;
    AudioStreamBasicDescription format = pcm_format(true);
    OSStatus status = AudioQueueNewOutput(&format, refill, output, NULL, NULL, 0, &output->queue);
    for (int i = 0; !status && i < 3; ++i) {
        AudioQueueBufferRef buffer;
        status = AudioQueueAllocateBuffer(output->queue, 480 * 8, &buffer);
        if (!status) {
            memset(buffer->mAudioData, 0, 480 * 8);
            buffer->mAudioDataByteSize = 480 * 8;
            status = AudioQueueEnqueueBuffer(output->queue, buffer, 0, NULL);
        }
    }
    if (!status) status = AudioQueueStart(output->queue, NULL);
    if (status) {
        if (output->queue) AudioQueueDispose(output->queue, true);
        free(output);
        return status;
    }
    *out = output;
    return noErr;
}

OSStatus mc_audio_output_error(Output *output) { return output->error; }
void mc_audio_output_close(Output *output) {
    AudioQueueDispose(output->queue, true);
    free(output);
}
