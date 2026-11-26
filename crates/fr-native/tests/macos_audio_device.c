// Check the HAL call shape and refusal cases without opening an audio device.
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#ifdef __APPLE__
#include <CoreAudio/CoreAudio.h>
#else
typedef uint32_t UInt32, AudioObjectID, AudioDeviceID;
typedef int32_t OSStatus;
typedef const void *CFStringRef;
typedef struct { UInt32 mSelector, mScope, mElement; } AudioObjectPropertyAddress;
enum { noErr, kAudioObjectUnknown = 0, kAudioObjectSystemObject,
       kAudioHardwarePropertyTranslateUIDToDevice, kAudioHardwarePropertyDefaultOutputDevice,
       kAudioObjectPropertyScopeGlobal,
       kAudioObjectPropertyElementMain };
#endif

static const char token = 0;
static unsigned calls, default_calls;
static AudioDeviceID default_id;
static OSStatus result;
static UInt32 response_size;
static AudioDeviceID response_id;

static OSStatus get_property(AudioObjectID object, const AudioObjectPropertyAddress *address,
                             UInt32 qualifier_size, const void *qualifier,
                             UInt32 *size, void *out) {
    ++calls;
    assert(object == kAudioObjectSystemObject);
    assert(address->mScope == kAudioObjectPropertyScopeGlobal);
    assert(address->mElement == kAudioObjectPropertyElementMain);
    if (address->mSelector == kAudioHardwarePropertyDefaultOutputDevice) {
        ++default_calls;
        assert(qualifier_size == 0 && !qualifier && *size == sizeof(AudioDeviceID));
        *(AudioDeviceID *)out = default_id;
        return noErr;
    }
    assert(address->mSelector == kAudioHardwarePropertyTranslateUIDToDevice);
    assert(qualifier_size == sizeof(CFStringRef));
    assert(qualifier && *(const CFStringRef *)qualifier == (CFStringRef)(const void *)&token);
    assert(*size == sizeof(AudioDeviceID));
    *(AudioDeviceID *)out = response_id;
    *size = response_size;
    return result;
}
#define AudioObjectGetPropertyData get_property
#include "../src/macos_audio_device.h"

int main(void) {
    CFStringRef uid = (CFStringRef)(const void *)&token;
    assert(fr_audio_device_for_uid(NULL) == kAudioObjectUnknown && calls == 0);
    response_size = sizeof(AudioDeviceID);
    response_id = 177;
    assert(fr_audio_device_for_uid(uid) == 177);
    response_id = kAudioObjectUnknown;
    assert(fr_audio_device_for_uid(uid) == kAudioObjectUnknown);
    response_id = 177;
    result = -1;
    assert(fr_audio_device_for_uid(uid) == kAudioObjectUnknown);
    result = noErr;
    --response_size;
    assert(fr_audio_device_for_uid(uid) == kAudioObjectUnknown && calls == 4);
    result = noErr;
    response_size = sizeof(AudioDeviceID);
    default_id = 42;
    assert(fr_audio_output_device(NULL) == 42);
    assert(fr_audio_output_device(uid) == 177);
    default_id = 43;
    assert(fr_audio_output_device(uid) == 177);
    response_id = kAudioObjectUnknown;
    assert(fr_audio_output_device(uid) == kAudioObjectUnknown);
    assert(default_calls == 1); // missing selected device cannot use the default
    assert(fr_audio_output_device(NULL) == 43);
    return 0;
}
