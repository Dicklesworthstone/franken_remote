// FrankenRemote Phase 0 Spike: Windows Desktop Duplication, D3D11 Video, and Hardware HEVC Probe
// Provenance: plan sections 8.3, 9.1, 10.3, 23 Phase 0; bead fr-p0-media-windows-fbt
#define WIN32_LEAN_AND_MEAN
#define INITGUID
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_6.h>
#include <mfapi.h>
#include <mftransform.h>
#include <mfidl.h>
#include <stdio.h>
#include <string.h>

#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "dxgi.lib")
#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "user32.lib")

// D3D11 Video Decoder HEVC profile GUIDs
// D3D11_DECODER_PROFILE_HEVC_VLD_MAIN = {5b11d51b-dd4c-47c0-ac5e-344cb38e60fe}
DEFINE_GUID(D3D11_DECODER_PROFILE_HEVC_VLD_MAIN_LOCAL,
    0x5b11d51b, 0xdd4c, 0x47c0, 0xac, 0x5e, 0x34, 0x4c, 0xb3, 0x8e, 0x60, 0xfe);

// D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10 = {107af0e0-ef1a-4d19-aba8-67a163073d13}
DEFINE_GUID(D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10_LOCAL,
    0x107af0e0, 0xef1a, 0x4d19, 0xab, 0xa8, 0x67, 0xa1, 0x63, 0x07, 0x3d, 0x13);

static const char* hresult_to_string(HRESULT hr) {
    switch (hr) {
        case S_OK: return "S_OK";
        case S_FALSE: return "S_FALSE";
        case E_ACCESSDENIED: return "E_ACCESSDENIED";
        case E_INVALIDARG: return "E_INVALIDARG";
        case E_NOINTERFACE: return "E_NOINTERFACE";
        case E_NOTIMPL: return "E_NOTIMPL";
        case DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: return "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE";
        case DXGI_ERROR_ACCESS_LOST: return "DXGI_ERROR_ACCESS_LOST";
        case DXGI_ERROR_UNSUPPORTED: return "DXGI_ERROR_UNSUPPORTED";
        case DXGI_ERROR_SESSION_DISCONNECTED: return "DXGI_ERROR_SESSION_DISCONNECTED";
        case DXGI_ERROR_DEVICE_REMOVED: return "DXGI_ERROR_DEVICE_REMOVED";
        case DXGI_ERROR_DEVICE_RESET: return "DXGI_ERROR_DEVICE_RESET";
        case DXGI_ERROR_NOT_FOUND: return "DXGI_ERROR_NOT_FOUND";
        default: return "UNKNOWN_ERROR";
    }
}

static void escape_json_string(const wchar_t* src, char* dst, size_t dst_size) {
    size_t out_idx = 0;
    while (*src && out_idx + 2 < dst_size) {
        if (*src == L'\\' || *src == L'"') {
            dst[out_idx++] = '\\';
            dst[out_idx++] = (char)*src;
        } else if (*src >= 32 && *src < 127) {
            dst[out_idx++] = (char)*src;
        } else {
            dst[out_idx++] = '?';
        }
        src++;
    }
    dst[out_idx] = '\0';
}

int main(int argc, char** argv) {
    if (argc > 1) {
        FILE* f = NULL;
        freopen_s(&f, argv[1], "w", stdout);
    }
    HRESULT hrCo = CoInitializeEx(NULL, COINIT_MULTITHREADED);

    DWORD session_id = 0;
    ProcessIdToSessionId(GetCurrentProcessId(), &session_id);

    printf("{\n");
    printf("  \"schema\": \"frankenremote.probe.windows_media.v1\",\n");
    printf("  \"session\": {\n");
    printf("    \"process_id\": %lu,\n", GetCurrentProcessId());
    printf("    \"session_id\": %lu,\n", session_id);
    printf("    \"is_session_zero\": %s\n", (session_id == 0) ? "true" : "false");
    printf("  },\n");

    // Enumerate DXGI adapters and outputs
    IDXGIFactory1* pFactory = NULL;
    HRESULT hr = CreateDXGIFactory1(IID_PPV_ARGS(&pFactory));
    printf("  \"dxgi_factory_hr\": \"0x%08lX\",\n", hr);

    printf("  \"adapters\": [\n");
    if (SUCCEEDED(hr) && pFactory) {
        UINT adapterIndex = 0;
        IDXGIAdapter1* pAdapter = NULL;
        BOOL firstAdapter = TRUE;

        while (pFactory->EnumAdapters1(adapterIndex++, &pAdapter) == S_OK) {
            if (!pAdapter) break;
            DXGI_ADAPTER_DESC1 desc;
            memset(&desc, 0, sizeof(desc));
            pAdapter->GetDesc1(&desc);

            char descEscaped[256];
            escape_json_string(desc.Description, descEscaped, sizeof(descEscaped));

            if (!firstAdapter) printf(",\n");
            firstAdapter = FALSE;

            printf("    {\n");
            printf("      \"index\": %u,\n", adapterIndex - 1);
            printf("      \"description\": \"%s\",\n", descEscaped);
            printf("      \"vendor_id\": \"0x%04X\",\n", desc.VendorId);
            printf("      \"device_id\": \"0x%04X\",\n", desc.DeviceId);
            printf("      \"subsys_id\": \"0x%08X\",\n", desc.SubSysId);
            printf("      \"revision\": %u,\n", desc.Revision);
            printf("      \"dedicated_video_memory_bytes\": %I64u,\n", (unsigned __int64)desc.DedicatedVideoMemory);
            printf("      \"dedicated_system_memory_bytes\": %I64u,\n", (unsigned __int64)desc.DedicatedSystemMemory);
            printf("      \"shared_system_memory_bytes\": %I64u,\n", (unsigned __int64)desc.SharedSystemMemory);
            printf("      \"is_software\": %s,\n", (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) ? "true" : "false");
            fflush(stdout);

            // Create D3D11 Device on this adapter
            D3D_FEATURE_LEVEL featureLevels[] = {
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0
            };
            D3D_FEATURE_LEVEL featureLevelOut = D3D_FEATURE_LEVEL_11_0;
            ID3D11Device* pDevice = NULL;
            ID3D11DeviceContext* pContext = NULL;
            HRESULT hrD3D = D3D11CreateDevice(
                pAdapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                NULL,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                featureLevels,
                ARRAYSIZE(featureLevels),
                D3D11_SDK_VERSION,
                &pDevice,
                &featureLevelOut,
                &pContext
            );
            printf("      \"d3d11_create_device_hr\": \"0x%08lX\",\n", hrD3D);
            printf("      \"d3d11_feature_level\": \"0x%04X\",\n", (UINT)featureLevelOut);

            // Check D3D11 Video Decoder support for HEVC
            BOOL hasHevcDecode = FALSE;
            BOOL hasHevcMain10Decode = FALSE;
            UINT decoderProfileCount = 0;
            if (SUCCEEDED(hrD3D) && pDevice) {
                ID3D11VideoDevice* pVideoDevice = NULL;
                if (SUCCEEDED(pDevice->QueryInterface(IID_PPV_ARGS(&pVideoDevice))) && pVideoDevice) {
                    decoderProfileCount = pVideoDevice->GetVideoDecoderProfileCount();
                    for (UINT p = 0; p < decoderProfileCount; p++) {
                        GUID profileGuid;
                        if (SUCCEEDED(pVideoDevice->GetVideoDecoderProfile(p, &profileGuid))) {
                            if (IsEqualGUID(profileGuid, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN_LOCAL)) {
                                hasHevcDecode = TRUE;
                            }
                            if (IsEqualGUID(profileGuid, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10_LOCAL)) {
                                hasHevcMain10Decode = TRUE;
                            }
                        }
                    }
                    pVideoDevice->Release();
                }
            }
            printf("      \"d3d11_video_decoder_profile_count\": %u,\n", decoderProfileCount);
            printf("      \"d3d11_hevc_main_decode_supported\": %s,\n", hasHevcDecode ? "true" : "false");
            printf("      \"d3d11_hevc_main10_decode_supported\": %s,\n", hasHevcMain10Decode ? "true" : "false");
            fflush(stdout);

            // Enumerate Outputs and probe Desktop Duplication
            printf("      \"outputs\": [\n");
            UINT outputIndex = 0;
            IDXGIOutput* pOutput = NULL;
            BOOL firstOutput = TRUE;
            HRESULT enumOutHr = S_OK;

            while ((enumOutHr = pAdapter->EnumOutputs(outputIndex++, &pOutput)) == S_OK) {
                if (!pOutput) break;
                DXGI_OUTPUT_DESC outDesc;
                memset(&outDesc, 0, sizeof(outDesc));
                pOutput->GetDesc(&outDesc);

                char outNameEscaped[128];
                escape_json_string(outDesc.DeviceName, outNameEscaped, sizeof(outNameEscaped));

                if (!firstOutput) printf(",\n");
                firstOutput = FALSE;

                printf("        {\n");
                printf("          \"index\": %u,\n", outputIndex - 1);
                printf("          \"device_name\": \"%s\",\n", outNameEscaped);
                printf("          \"attached_to_desktop\": %s,\n", outDesc.AttachedToDesktop ? "true" : "false");
                printf("          \"desktop_coordinates\": {\"left\": %ld, \"top\": %ld, \"right\": %ld, \"bottom\": %ld},\n",
                    outDesc.DesktopCoordinates.left, outDesc.DesktopCoordinates.top,
                    outDesc.DesktopCoordinates.right, outDesc.DesktopCoordinates.bottom);
                printf("          \"rotation\": %d,\n", (int)outDesc.Rotation);

                // Test Desktop Duplication
                HRESULT hrDup = E_FAIL;
                const char* dupReason = "device_creation_failed";
                HRESULT hrAcq = E_FAIL;
                const char* acqReason = "not_attempted";
                BOOL frameAcquired = FALSE;
                UINT texWidth = 0, texHeight = 0, texFormat = 0;
                BOOL cursorVisible = FALSE;
                LONG cursorX = 0, cursorY = 0;
                UINT dirtyRectCount = 0;
                double copyMicroseconds = 0.0;
                BOOL copySucceeded = FALSE;
                int sessionExhaustionLimit = 0;
                HRESULT hrExhaustion = S_OK;

                if (SUCCEEDED(hrD3D) && pDevice) {
                    IDXGIOutput1* pOutput1 = NULL;
                    if (SUCCEEDED(pOutput->QueryInterface(IID_PPV_ARGS(&pOutput1)))) {
                        IDXGIOutputDuplication* pDuplication = NULL;
                        hrDup = pOutput1->DuplicateOutput((IUnknown*)pDevice, &pDuplication);
                        dupReason = hresult_to_string(hrDup);

                        if (SUCCEEDED(hrDup) && pDuplication) {
                            // Test AcquireNextFrame with 1000ms timeout
                            DXGI_OUTDUPL_FRAME_INFO frameInfo;
                            memset(&frameInfo, 0, sizeof(frameInfo));
                            IDXGIResource* pDesktopResource = NULL;
                            hrAcq = pDuplication->AcquireNextFrame(1000, &frameInfo, &pDesktopResource);
                            acqReason = hresult_to_string(hrAcq);

                            if (SUCCEEDED(hrAcq) && pDesktopResource) {
                                frameAcquired = TRUE;
                                cursorVisible = frameInfo.PointerPosition.Visible;
                                cursorX = frameInfo.PointerPosition.Position.x;
                                cursorY = frameInfo.PointerPosition.Position.y;

                                ID3D11Texture2D* pDesktopTexture = NULL;
                                if (SUCCEEDED(pDesktopResource->QueryInterface(IID_PPV_ARGS(&pDesktopTexture)))) {
                                    D3D11_TEXTURE2D_DESC texDesc;
                                    pDesktopTexture->GetDesc(&texDesc);
                                    texWidth = texDesc.Width;
                                    texHeight = texDesc.Height;
                                    texFormat = texDesc.Format;

                                    // Read dirty rects
                                    RECT dirtyRects[32];
                                    UINT dirtyBytes = sizeof(dirtyRects);
                                    if (SUCCEEDED(pDuplication->GetFrameDirtyRects(sizeof(dirtyRects), dirtyRects, &dirtyBytes))) {
                                        dirtyRectCount = dirtyBytes / sizeof(RECT);
                                    }

                                    // Test copy to owned GPU surface
                                    D3D11_TEXTURE2D_DESC ownedDesc = texDesc;
                                    ownedDesc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
                                    ownedDesc.MiscFlags = 0;
                                    ID3D11Texture2D* pOwnedTexture = NULL;
                                    if (SUCCEEDED(pDevice->CreateTexture2D(&ownedDesc, NULL, &pOwnedTexture))) {
                                        LARGE_INTEGER freq, t0, t1;
                                        QueryPerformanceFrequency(&freq);
                                        QueryPerformanceCounter(&t0);
                                        pContext->CopyResource(pOwnedTexture, pDesktopTexture);
                                        QueryPerformanceCounter(&t1);
                                        copyMicroseconds = (double)(t1.QuadPart - t0.QuadPart) * 1000000.0 / (double)freq.QuadPart;
                                        copySucceeded = TRUE;
                                        pOwnedTexture->Release();
                                    }
                                    pDesktopTexture->Release();
                                }
                                pDesktopResource->Release();
                                // Prompt frame release
                                pDuplication->ReleaseFrame();
                            }

                            // Test duplication session exhaustion
                            IDXGIOutputDuplication* extraDup[8] = {0};
                            int extraCount = 0;
                            for (int e = 0; e < 8; e++) {
                                hrExhaustion = pOutput1->DuplicateOutput((IUnknown*)pDevice, &extraDup[e]);
                                if (SUCCEEDED(hrExhaustion)) {
                                    extraCount++;
                                } else {
                                    break;
                                }
                            }
                            sessionExhaustionLimit = extraCount + 1;
                            for (int e = 0; e < extraCount; e++) {
                                if (extraDup[e]) extraDup[e]->Release();
                            }

                            pDuplication->Release();
                        }
                        pOutput1->Release();
                    } else {
                        dupReason = "idxgioutput1_unsupported";
                    }
                }
                printf("          \"duplicate_output_hr\": \"0x%08lX\",\n", hrDup);
                printf("          \"duplicate_output_symbol\": \"%s\",\n", dupReason);
                printf("          \"duplicate_output_succeeded\": %s,\n", SUCCEEDED(hrDup) ? "true" : "false");
                printf("          \"acquire_frame_hr\": \"0x%08lX\",\n", hrAcq);
                printf("          \"acquire_frame_symbol\": \"%s\",\n", acqReason);
                printf("          \"frame_acquired\": %s,\n", frameAcquired ? "true" : "false");
                if (frameAcquired) {
                    printf("          \"captured_texture\": {\"width\": %u, \"height\": %u, \"dxgi_format\": %u},\n",
                        texWidth, texHeight, texFormat);
                    printf("          \"cursor\": {\"visible\": %s, \"x\": %ld, \"y\": %ld},\n",
                        cursorVisible ? "true" : "false", cursorX, cursorY);
                    printf("          \"dirty_rect_count\": %u,\n", dirtyRectCount);
                    printf("          \"copy_to_owned_surface\": {\"succeeded\": %s, \"gpu_copy_us\": %.2f},\n",
                        copySucceeded ? "true" : "false", copyMicroseconds);
                }
                printf("          \"session_exhaustion\": {\"max_concurrent_sessions\": %d, \"exhaustion_hr\": \"0x%08lX\", \"exhaustion_symbol\": \"%s\"},\n",
                    sessionExhaustionLimit, hrExhaustion, hresult_to_string(hrExhaustion));

                // Test DuplicateOutput1 (DXGI 1.5+) for format selection
                HRESULT hrDup1 = E_FAIL;
                const char* dup1Reason = "idxgioutput5_unsupported";
                if (SUCCEEDED(hrD3D) && pDevice) {
                    IDXGIOutput5* pOutput5 = NULL;
                    if (SUCCEEDED(pOutput->QueryInterface(IID_PPV_ARGS(&pOutput5)))) {
                        DXGI_FORMAT formats[] = { DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM };
                        IDXGIOutputDuplication* pDuplication = NULL;
                        hrDup1 = pOutput5->DuplicateOutput1((IUnknown*)pDevice, 0, ARRAYSIZE(formats), formats, &pDuplication);
                        dup1Reason = hresult_to_string(hrDup1);
                        if (SUCCEEDED(hrDup1) && pDuplication) {
                            pDuplication->Release();
                        }
                        pOutput5->Release();
                    }
                }
                printf("          \"duplicate_output1_hr\": \"0x%08lX\",\n", hrDup1);
                printf("          \"duplicate_output1_symbol\": \"%s\",\n", dup1Reason);
                printf("          \"duplicate_output1_succeeded\": %s\n", SUCCEEDED(hrDup1) ? "true" : "false");
                printf("        }");

                pOutput->Release();
            }
            printf("\n      ]\n");
            printf("    }");

            if (pContext) pContext->Release();
            if (pDevice) pDevice->Release();
            pAdapter->Release();
        }
        pFactory->Release();
    }
    printf("\n  ],\n");

    // Media Foundation Hardware Codecs
    HRESULT hrMf = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
    printf("  \"media_foundation\": {\n");
    printf("    \"startup_hr\": \"0x%08lX\",\n", hrMf);

    if (SUCCEEDED(hrMf)) {
        // Enumerate Hardware HEVC Encoders
        MFT_REGISTER_TYPE_INFO encIn = { MFMediaType_Video, MFVideoFormat_NV12 };
        MFT_REGISTER_TYPE_INFO encOut = { MFMediaType_Video, MFVideoFormat_HEVC };
        IMFActivate** ppEncActivates = NULL;
        UINT32 encCount = 0;
        HRESULT hrEnc = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            &encIn,
            &encOut,
            &ppEncActivates,
            &encCount
        );

        printf("    \"hardware_hevc_encoders_hr\": \"0x%08lX\",\n", hrEnc);
        printf("    \"hardware_hevc_encoder_count\": %u,\n", encCount);
        printf("    \"hardware_hevc_encoders\": [\n");
        for (UINT32 i = 0; i < encCount; i++) {
            WCHAR friendlyName[256] = {0};
            UINT32 nameLen = 0;
            HRESULT hrName = ppEncActivates[i]->GetString(MFT_FRIENDLY_NAME_Attribute, friendlyName, ARRAYSIZE(friendlyName), &nameLen);
            if (FAILED(hrName)) {
                // Fallback to CLSID
                GUID clsid;
                if (SUCCEEDED(ppEncActivates[i]->GetGUID(MFT_TRANSFORM_CLSID_Attribute, &clsid))) {
                    StringFromGUID2(clsid, friendlyName, ARRAYSIZE(friendlyName));
                }
            }
            char nameEscaped[256];
            escape_json_string(friendlyName, nameEscaped, sizeof(nameEscaped));
            printf("      \"%s\"%s\n", nameEscaped, (i + 1 < encCount) ? "," : "");
            ppEncActivates[i]->Release();
        }
        if (ppEncActivates) CoTaskMemFree(ppEncActivates);
        printf("    ],\n");

        // Enumerate Hardware HEVC Decoders
        MFT_REGISTER_TYPE_INFO decIn = { MFMediaType_Video, MFVideoFormat_HEVC };
        MFT_REGISTER_TYPE_INFO decOut = { MFMediaType_Video, MFVideoFormat_NV12 };
        IMFActivate** ppDecActivates = NULL;
        UINT32 decCount = 0;
        HRESULT hrDec = MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            &decIn,
            &decOut,
            &ppDecActivates,
            &decCount
        );

        printf("    \"hardware_hevc_decoders_hr\": \"0x%08lX\",\n", hrDec);
        printf("    \"hardware_hevc_decoder_count\": %u,\n", decCount);
        printf("    \"hardware_hevc_decoders\": [\n");
        for (UINT32 i = 0; i < decCount; i++) {
            WCHAR friendlyName[256] = {0};
            UINT32 nameLen = 0;
            HRESULT hrName = ppDecActivates[i]->GetString(MFT_FRIENDLY_NAME_Attribute, friendlyName, ARRAYSIZE(friendlyName), &nameLen);
            if (FAILED(hrName)) {
                GUID clsid;
                if (SUCCEEDED(ppDecActivates[i]->GetGUID(MFT_TRANSFORM_CLSID_Attribute, &clsid))) {
                    StringFromGUID2(clsid, friendlyName, ARRAYSIZE(friendlyName));
                }
            }
            char nameEscaped[256];
            escape_json_string(friendlyName, nameEscaped, sizeof(nameEscaped));
            printf("      \"%s\"%s\n", nameEscaped, (i + 1 < decCount) ? "," : "");
            ppDecActivates[i]->Release();
        }
        if (ppDecActivates) CoTaskMemFree(ppDecActivates);
        printf("    ]\n");

        MFShutdown();
    } else {
        printf("    \"hardware_hevc_encoder_count\": 0,\n");
        printf("    \"hardware_hevc_decoder_count\": 0\n");
    }
    printf("  }\n");
    printf("}\n");

    if (SUCCEEDED(hrCo)) CoUninitialize();
    return 0;
}
