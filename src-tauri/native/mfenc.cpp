#include "mfenc.h"

#include <codecapi.h>
#include <mferror.h>
#include <strmif.h>

#include <vector>

using Microsoft::WRL::ComPtr;

namespace {

std::string Hex(HRESULT hr) {
    char b[16];
    snprintf(b, sizeof b, "0x%08lX", static_cast<unsigned long>(hr));
    return b;
}

std::string Narrow(const wchar_t* w) {
    if (!w) return {};
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, nullptr, 0, nullptr, nullptr);
    std::string s(n > 0 ? n - 1 : 0, '\0');
    if (n > 1) WideCharToMultiByte(CP_UTF8, 0, w, -1, s.data(), n, nullptr, nullptr);
    return s;
}

void SetU4(ICodecAPI* api, const GUID& key, ULONG v) {
    VARIANT var;
    VariantInit(&var);
    var.vt = VT_UI4;
    var.ulVal = v;
    api->SetValue(&key, &var);
}

void SetBool(ICodecAPI* api, const GUID& key, bool v) {
    VARIANT var;
    VariantInit(&var);
    var.vt = VT_BOOL;
    var.boolVal = v ? VARIANT_TRUE : VARIANT_FALSE;
    api->SetValue(&key, &var);
}

LUID AdapterLuid(ID3D11Device* dev) {
    LUID luid{};
    ComPtr<IDXGIDevice> dxgi;
    ComPtr<IDXGIAdapter> adapter;
    DXGI_ADAPTER_DESC desc{};
    if (SUCCEEDED(dev->QueryInterface(IID_PPV_ARGS(&dxgi))) && SUCCEEDED(dxgi->GetAdapter(&adapter)) && SUCCEEDED(adapter->GetDesc(&desc)))
        luid = desc.AdapterLuid;
    return luid;
}

// Whether an IRAP / IDR NAL is in the access unit (backup for the clean-point flag).
bool HasKeyNal(const uint8_t* d, size_t n, int codec) {
    for (size_t i = 0; i + 3 < n; i++) {
        if (d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1) {
            uint8_t b = d[i + 3];
            if (codec == 0 && (b & 0x1f) == 5) return true;
            if (codec == 1) {
                int t = (b >> 1) & 0x3f;
                if (t >= 16 && t <= 21) return true;
            }
        }
    }
    return false;
}

}  // namespace

MfEncoder::~MfEncoder() { Stop(); }

size_t MfEncoder::Pending() {
    std::lock_guard<std::mutex> lk(mu_);
    return queue_.size();
}

bool MfEncoder::Init(ID3D11Device* dev, UINT w, UINT h, UINT fps, UINT kbps, UINT max_kbps, int codec, Output out, std::string& err) {
    w_ = w, h_ = h, fps_ = fps, kbps_ = kbps, max_kbps_ = max_kbps, codec_ = codec, out_ = std::move(out);
    HRESULT hr = MFCreateDXGIDeviceManager(&reset_token_, &manager_);
    if (FAILED(hr) || FAILED(hr = manager_->ResetDevice(dev, reset_token_))) {
        err = "Media Foundation couldn't use the GPU (" + Hex(hr) + ")";
        return false;
    }
    MFT_REGISTER_TYPE_INFO in{MFMediaType_Video, MFVideoFormat_NV12};
    MFT_REGISTER_TYPE_INFO outType{MFMediaType_Video, codec == 1 ? MFVideoFormat_HEVC : MFVideoFormat_H264};
    IMFActivate** acts = nullptr;
    UINT32 count = 0;
    hr = MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, &in, &outType, &acts, &count);
    if (FAILED(hr) || count == 0) {
        err = codec == 1 ? "No hardware HEVC encoder was found" : "No hardware H.264 encoder was found";
        return false;
    }
    // Prefer the encoder on the GPU we capture with.
    LUID luid = AdapterLuid(dev);
    UINT pick = 0;
    for (UINT i = 0; i < count; i++) {
        UINT64 v = 0;
        UINT32 size = 0;
        LUID l{};
        if (SUCCEEDED(acts[i]->GetBlob(MFT_ENUM_ADAPTER_LUID, reinterpret_cast<UINT8*>(&l), sizeof l, &size)) && size == sizeof l) {
            if (l.LowPart == luid.LowPart && l.HighPart == luid.HighPart) { pick = i; break; }
        } else if (SUCCEEDED(acts[i]->GetUINT64(MFT_ENUM_ADAPTER_LUID, &v))) {
            if (v == ((static_cast<UINT64>(static_cast<UINT32>(luid.HighPart)) << 32) | luid.LowPart)) { pick = i; break; }
        }
    }
    std::string lastErr;
    // Try the matching one first, then the others.
    std::vector<UINT> order{pick};
    for (UINT i = 0; i < count; i++) if (i != pick) order.push_back(i);
    bool ok = false;
    for (UINT i : order) {
        WCHAR* friendly = nullptr;
        UINT32 len = 0;
        acts[i]->GetAllocatedString(MFT_FRIENDLY_NAME_Attribute, &friendly, &len);
        name_ = Narrow(friendly);
        CoTaskMemFree(friendly);
        mft_.Reset();
        if (FAILED(hr = acts[i]->ActivateObject(IID_PPV_ARGS(&mft_)))) { lastErr = "couldn't start " + name_ + " (" + Hex(hr) + ")"; continue; }
        if (Negotiate(lastErr)) { ok = true; break; }
        acts[i]->ShutdownObject();
    }
    for (UINT i = 0; i < count; i++) acts[i]->Release();
    CoTaskMemFree(acts);
    if (!ok) {
        err = "The hardware encoder didn't accept the settings: " + lastErr;
        return false;
    }
    thread_ = std::thread([this] { Run(); });
    return true;
}

bool MfEncoder::Negotiate(std::string& err) {
    HRESULT hr;
    ComPtr<IMFAttributes> attrs;
    if (SUCCEEDED(mft_->GetAttributes(&attrs))) {
        UINT32 async = 0;
        attrs->GetUINT32(MF_TRANSFORM_ASYNC, &async);
        if (!async) { err = name_ + " isn't an async hardware encoder"; return false; }
        attrs->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE);
        attrs->SetUINT32(MF_LOW_LATENCY, TRUE);
    }
    if (FAILED(hr = mft_->ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, reinterpret_cast<ULONG_PTR>(manager_.Get())))) {
        err = name_ + " can't take GPU frames (" + Hex(hr) + ")";
        return false;
    }
    ComPtr<ICodecAPI> api;
    if (SUCCEEDED(mft_.As(&api))) {
        SetU4(api.Get(), CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_PeakConstrainedVBR);
        SetU4(api.Get(), CODECAPI_AVEncCommonMeanBitRate, kbps_ * 1000);
        SetU4(api.Get(), CODECAPI_AVEncCommonMaxBitRate, max_kbps_ * 1000);
        SetU4(api.Get(), CODECAPI_AVEncCommonBufferSize, max_kbps_ * 2000);
        SetU4(api.Get(), CODECAPI_AVEncMPVGOPSize, fps_);  // a keyframe every second
        SetU4(api.Get(), CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        SetU4(api.Get(), CODECAPI_AVEncCommonQualityVsSpeed, 70);
        SetBool(api.Get(), CODECAPI_AVLowLatencyMode, true);
    }
    // Output first, then input (the order encoders expect).
    ComPtr<IMFMediaType> outType;
    MFCreateMediaType(&outType);
    outType->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    outType->SetGUID(MF_MT_SUBTYPE, codec_ == 1 ? MFVideoFormat_HEVC : MFVideoFormat_H264);
    outType->SetUINT32(MF_MT_AVG_BITRATE, kbps_ * 1000);
    MFSetAttributeSize(outType.Get(), MF_MT_FRAME_SIZE, w_, h_);
    MFSetAttributeRatio(outType.Get(), MF_MT_FRAME_RATE, fps_, 1);
    MFSetAttributeRatio(outType.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
    outType->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    outType->SetUINT32(MF_MT_MPEG2_PROFILE, codec_ == 1 ? eAVEncH265VProfile_Main_420_8 : eAVEncH264VProfile_High);
    outType->SetUINT32(MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235);
    outType->SetUINT32(MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709);
    if (FAILED(hr = mft_->SetOutputType(0, outType.Get(), 0))) {
        err = name_ + " refused " + std::to_string(w_) + "x" + std::to_string(h_) + " @ " + std::to_string(fps_) + " (" + Hex(hr) + ")";
        return false;
    }
    bool inOk = false;
    for (DWORD i = 0;; i++) {
        ComPtr<IMFMediaType> t;
        if (FAILED(mft_->GetInputAvailableType(0, i, &t))) break;
        GUID sub{};
        t->GetGUID(MF_MT_SUBTYPE, &sub);
        if (sub != MFVideoFormat_NV12) continue;
        MFSetAttributeSize(t.Get(), MF_MT_FRAME_SIZE, w_, h_);
        MFSetAttributeRatio(t.Get(), MF_MT_FRAME_RATE, fps_, 1);
        t->SetUINT32(MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235);
        t->SetUINT32(MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709);
        if (SUCCEEDED(hr = mft_->SetInputType(0, t.Get(), 0))) { inOk = true; break; }
    }
    if (!inOk) {
        err = name_ + " doesn't take NV12 frames (" + Hex(hr) + ")";
        return false;
    }
    MFT_OUTPUT_STREAM_INFO info{};
    if (SUCCEEDED(mft_->GetOutputStreamInfo(0, &info))) {
        provides_samples_ = (info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
        out_size_ = info.cbSize;
    }
    if (FAILED(hr = mft_.As(&events_))) { err = name_ + " has no events (" + Hex(hr) + ")"; return false; }
    mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    return true;
}

bool MfEncoder::Submit(ID3D11Texture2D* nv12, int64_t time_hns, int64_t dur_hns) {
    if (failed_ || stop_) return false;
    {
        std::lock_guard<std::mutex> lk(mu_);
        if (queue_.size() >= 4) return false;
    }
    ComPtr<IMFMediaBuffer> buf;
    ComPtr<IMFSample> sample;
    if (FAILED(MFCreateDXGISurfaceBuffer(__uuidof(ID3D11Texture2D), nv12, 0, FALSE, &buf))) return false;
    // The DXGI buffer's length must be set, or some encoders read 0 bytes.
    ComPtr<IMF2DBuffer> b2;
    DWORD len = 0;
    if (SUCCEEDED(buf.As(&b2)) && SUCCEEDED(b2->GetContiguousLength(&len))) buf->SetCurrentLength(len);
    if (FAILED(MFCreateSample(&sample)) || FAILED(sample->AddBuffer(buf.Get()))) return false;
    sample->SetSampleTime(time_hns);
    sample->SetSampleDuration(dur_hns);
    {
        std::lock_guard<std::mutex> lk(mu_);
        queue_.push_back(sample);
    }
    cv_.notify_one();
    return true;
}

void MfEncoder::Run() {
    MFStartup(MF_VERSION, MFSTARTUP_LITE);
    while (!stop_) {
        ComPtr<IMFMediaEvent> ev;
        HRESULT hr = events_->GetEvent(0, &ev);
        if (FAILED(hr)) {
            if (!stop_) { error_ = "The encoder stopped (" + Hex(hr) + ")"; failed_ = true; }
            break;
        }
        MediaEventType type = MEUnknown;
        ev->GetType(&type);
        if (type == METransformNeedInput) {
            ComPtr<IMFSample> s;
            {
                std::unique_lock<std::mutex> lk(mu_);
                cv_.wait(lk, [&] { return stop_ || !queue_.empty(); });
                if (stop_) break;
                s = queue_.front();
                queue_.pop_front();
            }
            hr = mft_->ProcessInput(0, s.Get(), 0);
            if (FAILED(hr) && hr != MF_E_NOTACCEPTING) { error_ = "The encoder refused a frame (" + Hex(hr) + ")"; failed_ = true; break; }
        } else if (type == METransformHaveOutput) {
            HandleOutput();
            if (failed_) break;
        } else if (type == METransformDrainComplete) {
            break;
        }
    }
    MFShutdown();
}

void MfEncoder::HandleOutput() {
    MFT_OUTPUT_DATA_BUFFER out{};
    ComPtr<IMFSample> own;
    if (!provides_samples_) {
        ComPtr<IMFMediaBuffer> b;
        MFCreateMemoryBuffer(out_size_ ? out_size_ : w_ * h_ * 2, &b);
        MFCreateSample(&own);
        own->AddBuffer(b.Get());
        out.pSample = own.Get();
    }
    DWORD status = 0;
    HRESULT hr = mft_->ProcessOutput(0, 1, &out, &status);
    if (out.pEvents) out.pEvents->Release();
    if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
        // The encoder settled its output type: take the one it offers.
        ComPtr<IMFMediaType> t;
        if (SUCCEEDED(mft_->GetOutputAvailableType(0, 0, &t))) mft_->SetOutputType(0, t.Get(), 0);
        return;
    }
    if (FAILED(hr)) {
        if (hr != MF_E_TRANSFORM_NEED_MORE_INPUT) { error_ = "The encoder failed (" + Hex(hr) + ")"; failed_ = true; }
        return;
    }
    IMFSample* s = out.pSample;
    if (!s) return;
    LONGLONG t = 0;
    s->GetSampleTime(&t);
    UINT32 clean = 0;
    s->GetUINT32(MFSampleExtension_CleanPoint, &clean);
    ComPtr<IMFMediaBuffer> buf;
    if (SUCCEEDED(s->ConvertToContiguousBuffer(&buf))) {
        BYTE* p = nullptr;
        DWORD len = 0;
        if (SUCCEEDED(buf->Lock(&p, nullptr, &len))) {
            bool key = clean != 0 || HasKeyNal(p, len, codec_);
            if (len) out_(p, len, t, key);
            buf->Unlock();
        }
    }
    if (provides_samples_) s->Release();
}

void MfEncoder::Stop() {
    if (stop_.exchange(true)) {
        if (thread_.joinable()) thread_.join();
        return;
    }
    cv_.notify_all();
    if (mft_) {
        // Unblocks GetEvent.
        ComPtr<IMFShutdown> sd;
        if (SUCCEEDED(mft_.As(&sd))) sd->Shutdown();
    }
    if (thread_.joinable()) thread_.join();
    if (mft_) {
        mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        mft_.Reset();
    }
    events_.Reset();
    std::lock_guard<std::mutex> lk(mu_);
    queue_.clear();
}
