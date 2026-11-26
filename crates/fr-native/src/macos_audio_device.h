#ifndef FR_MACOS_AUDIO_DEVICE_H
#define FR_MACOS_AUDIO_DEVICE_H

// Include after CoreAudio/CoreAudio.h. HAL accepts a CFStringRef qualifier
// and writes one AudioDeviceID. Unknown UIDs may succeed with an unknown ID.
static inline AudioDeviceID fr_audio_device_for_uid(CFStringRef uid) {
    if (!uid) return kAudioObjectUnknown;
    const AudioObjectPropertyAddress address = {
        kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain
    };
    AudioDeviceID device = kAudioObjectUnknown;
    UInt32 size = sizeof device;
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address,
                                                 sizeof uid, &uid, &size, &device);
    return status == noErr && size == sizeof device ? device : kAudioObjectUnknown;
}

#endif
