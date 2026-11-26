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
       kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal,
       kAudioObjectPropertyElementMain };
#endif

static const char token = 0;
static unsigned calls;
static OSStatus result;
static UInt32 response_size;
static AudioDeviceID response_id;

static OSStatus get_property(AudioObjectID object, const AudioObjectPropertyAddress *address,
                             UInt32 qualifier_size, const void *qualifier,
                             UInt32 *size, void *out) {
    ++calls;
    assert(object == kAudioObjectSystemObject);
    assert(address->mSelector == kAudioHardwarePropertyTranslateUIDToDevice);
    assert(address->mScope == kAudioObjectPropertyScopeGlobal);
    assert(address->mElement == kAudioObjectPropertyElementMain);
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
    return 0;
}
