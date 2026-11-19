// Hardware video encoder through Media Foundation (async MFT): takes NV12 D3D11
// textures, gives Annex B access units. Works with NVIDIA, AMD and Intel drivers.
#pragma once
#include <d3d11.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>
#include <wrl/client.h>

#include <atomic>
#include <condition_variable>
#include <deque>
#include <functional>
#include <mutex>
#include <string>
#include <thread>

class MfEncoder {
public:
    using Output = std::function<void(const uint8_t*, size_t, int64_t, bool)>;

    ~MfEncoder();
    // codec: 0 = H.264, 1 = HEVC.
    bool Init(ID3D11Device* dev, UINT w, UINT h, UINT fps, UINT kbps, UINT max_kbps, int codec, Output out, std::string& err);
    // Queues a frame. False when the encoder is too far behind (the frame is dropped).
    bool Submit(ID3D11Texture2D* nv12, int64_t time_hns, int64_t dur_hns);
    void Stop();
    const std::string& Name() const { return name_; }
    bool Failed() const { return failed_; }
    const std::string& Error() const { return error_; }
    size_t Pending();

private:
    void Run();
    bool Negotiate(std::string& err);
    void HandleOutput();

    Microsoft::WRL::ComPtr<IMFTransform> mft_;
    Microsoft::WRL::ComPtr<IMFDXGIDeviceManager> manager_;
    Microsoft::WRL::ComPtr<IMFMediaEventGenerator> events_;
    UINT reset_token_ = 0;
    UINT w_ = 0, h_ = 0, fps_ = 60, kbps_ = 0, max_kbps_ = 0;
    int codec_ = 0;
    bool provides_samples_ = true;
    DWORD out_size_ = 0;
    Output out_;
    std::string name_;
    std::string error_;
    std::atomic<bool> failed_{false};

    std::mutex mu_;
    std::condition_variable cv_;
    std::deque<Microsoft::WRL::ComPtr<IMFSample>> queue_;
    int need_input_ = 0;
    std::atomic<bool> stop_{false};
    std::thread thread_;
};
