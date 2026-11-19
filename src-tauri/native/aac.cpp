// PCM -> AAC-LC with Windows' own encoder (Media Foundation). See clipcore.h.
#include "clipcore.h"

#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mftransform.h>
#include <wmcodecdsp.h>
#include <wrl/client.h>

#include <algorithm>
#include <cstring>
#include <string>

using Microsoft::WRL::ComPtr;

namespace {

void Copy(char* dst, size_t n, const std::string& s) {
    if (!dst || !n) return;
    size_t k = std::min(n - 1, s.size());
    memcpy(dst, s.data(), k);
    dst[k] = 0;
}

int RateIndex(uint32_t rate) {
    static const uint32_t rates[] = {96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000};
    for (int i = 0; i < 12; i++)
        if (rates[i] == rate) return i;
    return 3;
}

}  // namespace

extern "C" int32_t cc_aac_encode(const int16_t* pcm, size_t frames, uint32_t channels, uint32_t rate, uint32_t kbps,
                                 cc_packet_fn on_frame, void* user, uint8_t asc[2], char* err, size_t errlen) {
    if (channels != 1 && channels != 2) { Copy(err, errlen, "AAC: only mono or stereo"); return -1; }
    HRESULT co = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    struct Co { HRESULT hr; ~Co() { if (SUCCEEDED(hr)) CoUninitialize(); } } coGuard{co};
    MFStartup(MF_VERSION, MFSTARTUP_LITE);
    struct Done { ~Done() { MFShutdown(); } } done;
    ComPtr<IMFTransform> mft;
    HRESULT hr = CoCreateInstance(CLSID_AACMFTEncoder, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&mft));
    if (FAILED(hr)) { Copy(err, errlen, "Windows' AAC encoder isn't available"); return -1; }
    // The encoder takes 12000, 16000, 20000 or 24000 bytes a second.
    UINT32 bytes = kbps >= 176 ? 24000 : kbps >= 144 ? 20000 : kbps >= 112 ? 16000 : 12000;
    ComPtr<IMFMediaType> out;
    MFCreateMediaType(&out);
    out->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
    out->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_AAC);
    out->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    out->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, rate);
    out->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, channels);
    out->SetUINT32(MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes);
    out->SetUINT32(MF_MT_AAC_PAYLOAD_TYPE, 0);  // raw frames
    out->SetUINT32(MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29);
    ComPtr<IMFMediaType> in;
    MFCreateMediaType(&in);
    in->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
    in->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_PCM);
    in->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    in->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, rate);
    in->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, channels);
    in->SetUINT32(MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2);
    in->SetUINT32(MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2);
    if (FAILED(hr = mft->SetOutputType(0, out.Get(), 0)) || FAILED(hr = mft->SetInputType(0, in.Get(), 0))) {
        Copy(err, errlen, "AAC encoder refused the format");
        return -1;
    }
    // AudioSpecificConfig: AAC-LC, rate index, channels.
    int ri = RateIndex(rate);
    asc[0] = static_cast<uint8_t>((2 << 3) | (ri >> 1));
    asc[1] = static_cast<uint8_t>(((ri & 1) << 7) | (channels << 3));

    MFT_OUTPUT_STREAM_INFO info{};
    mft->GetOutputStreamInfo(0, &info);
    const DWORD outSize = std::max<DWORD>(info.cbSize, 8192);
    mft->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    mft->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

    int64_t frameNo = 0;
    auto drainOutput = [&]() -> bool {
        for (;;) {
            ComPtr<IMFSample> s;
            ComPtr<IMFMediaBuffer> b;
            MFCreateMemoryBuffer(outSize, &b);
            MFCreateSample(&s);
            s->AddBuffer(b.Get());
            MFT_OUTPUT_DATA_BUFFER o{};
            o.pSample = s.Get();
            DWORD status = 0;
            HRESULT r = mft->ProcessOutput(0, 1, &o, &status);
            if (o.pEvents) o.pEvents->Release();
            if (r == MF_E_TRANSFORM_NEED_MORE_INPUT) return true;
            if (FAILED(r)) return false;
            ComPtr<IMFMediaBuffer> cb;
            if (SUCCEEDED(s->ConvertToContiguousBuffer(&cb))) {
                BYTE* p = nullptr;
                DWORD len = 0;
                if (SUCCEEDED(cb->Lock(&p, nullptr, &len))) {
                    if (len) on_frame(user, p, len, frameNo * 1024 * 10'000'000LL / rate, 1);
                    frameNo++;
                    cb->Unlock();
                }
            }
        }
    };

    const size_t chunk = 1024 * 16;  // frames per input buffer
    for (size_t pos = 0; pos < frames; pos += chunk) {
        size_t n = std::min(chunk, frames - pos);
        DWORD len = static_cast<DWORD>(n * channels * 2);
        ComPtr<IMFMediaBuffer> b;
        ComPtr<IMFSample> s;
        MFCreateMemoryBuffer(len, &b);
        BYTE* p = nullptr;
        b->Lock(&p, nullptr, nullptr);
        memcpy(p, pcm + pos * channels, len);
        b->Unlock();
        b->SetCurrentLength(len);
        MFCreateSample(&s);
        s->AddBuffer(b.Get());
        s->SetSampleTime(static_cast<LONGLONG>(pos) * 10'000'000 / rate);
        s->SetSampleDuration(static_cast<LONGLONG>(n) * 10'000'000 / rate);
        hr = mft->ProcessInput(0, s.Get(), 0);
        if (hr == MF_E_NOTACCEPTING) {
            if (!drainOutput()) { Copy(err, errlen, "AAC encoding failed"); return -1; }
            hr = mft->ProcessInput(0, s.Get(), 0);
        }
        if (FAILED(hr) || !drainOutput()) { Copy(err, errlen, "AAC encoding failed"); return -1; }
    }
    mft->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    drainOutput();
    mft->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    return 0;
}
