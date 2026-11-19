// clipcore engine: capture -> pacer -> GPU convert -> encoder. See clipcore.h.
#include "clipcore.h"
#include "mfenc.h"

#include <d3d11_4.h>
#include <dxgi1_2.h>
#include <windows.graphics.capture.interop.h>
#include <windows.graphics.directx.direct3d11.interop.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Metadata.h>
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>
#include <winrt/Windows.Graphics.DirectX.h>

#include <algorithm>
#include <atomic>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

using Microsoft::WRL::ComPtr;
namespace wgc = winrt::Windows::Graphics::Capture;
namespace wdx = winrt::Windows::Graphics::DirectX;

namespace {

int64_t QpcHns() {
    static const int64_t freq = [] { LARGE_INTEGER f; QueryPerformanceFrequency(&f); return f.QuadPart; }();
    LARGE_INTEGER c;
    QueryPerformanceCounter(&c);
    return static_cast<int64_t>(static_cast<long double>(c.QuadPart) * 10'000'000 / freq);
}

void Copy(char* dst, size_t n, const std::string& s) {
    if (!dst || !n) return;
    size_t k = std::min(n - 1, s.size());
    memcpy(dst, s.data(), k);
    dst[k] = 0;
}

std::string Hex(HRESULT hr) {
    char b[16];
    snprintf(b, sizeof b, "0x%08lX", static_cast<unsigned long>(hr));
    return b;
}

// The GPU that drives the monitor the window/monitor is on.
ComPtr<IDXGIAdapter1> AdapterFor(HMONITOR mon) {
    ComPtr<IDXGIFactory1> f;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&f)))) return nullptr;
    ComPtr<IDXGIAdapter1> a;
    for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; i++) {
        ComPtr<IDXGIOutput> o;
        for (UINT j = 0; a->EnumOutputs(j, &o) != DXGI_ERROR_NOT_FOUND; j++) {
            DXGI_OUTPUT_DESC d{};
            if (SUCCEEDED(o->GetDesc(&d)) && d.Monitor == mon) return a;
            o.Reset();
        }
        a.Reset();
    }
    return nullptr;
}

}  // namespace

struct cc_engine {
    cc_options opt{};
    // GPU
    ComPtr<ID3D11Device> dev;
    ComPtr<ID3D11DeviceContext> ctx;
    ComPtr<ID3D11VideoDevice> vdev;
    ComPtr<ID3D11VideoContext> vctx;
    winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice rtdev{nullptr};
    std::mutex gpu;  // our own use of the immediate context

    // Capture
    wgc::GraphicsCaptureItem item{nullptr};
    wgc::Direct3D11CaptureFramePool pool{nullptr};
    wgc::GraphicsCaptureSession session{nullptr};
    winrt::event_token arrived{};
    winrt::Windows::Graphics::SizeInt32 poolSize{};
    ComPtr<ID3D11Texture2D> latest;  // newest captured frame (content size)
    UINT latestW = 0, latestH = 0;
    uint64_t latestGen = 0;  // bumped when `latest` is recreated
    bool haveFrame = false;
    std::atomic<int64_t> lastCapture{0};
    std::atomic<uint64_t> captured{0};

    // Convert / scale
    UINT outW = 0, outH = 0;
    ComPtr<ID3D11VideoProcessorEnumerator> vpEnum;
    ComPtr<ID3D11VideoProcessor> vp;
    UINT vpInW = 0, vpInH = 0;
    ComPtr<ID3D11VideoProcessorInputView> inView;
    uint64_t inViewGen = ~0ull;
    static constexpr int kPool = 8;
    ComPtr<ID3D11Texture2D> nv12[kPool];
    ComPtr<ID3D11VideoProcessorOutputView> outView[kPool];
    int next = 0;

    // Killfeed tap: two staging textures, read one tap later (no GPU stall).
    ComPtr<ID3D11Texture2D> tapStage[2];
    UINT tapW[2] = {}, tapH[2] = {};
    int64_t tapTime[2] = {};
    bool tapPending[2] = {};
    int tapSlot = 0;
    std::vector<uint8_t> tapOut;

    // Encode
    MfEncoder enc;
    std::atomic<uint64_t> encoded{0}, repeated{0}, dropped{0};
    std::atomic<int64_t> lastPacket{0};

    // Pacer
    std::thread pacer;
    std::atomic<bool> stop{false};
    std::atomic<bool> failed{false};
    std::mutex errMu;
    std::string error;
    uint64_t frameNo = 0;
    bool newSinceTick = false;

    void Fail(const std::string& e) {
        std::lock_guard<std::mutex> lk(errMu);
        if (error.empty()) error = e;
        failed = true;
    }

    bool Init(std::string& err);
    void OnFrame(wgc::Direct3D11CaptureFramePool const& sender);
    void Pace();
    void Tick(int64_t t, int64_t dur);
    bool EnsureVp(UINT inW, UINT inH);
    void Tap(int64_t t);
    void Shutdown();
};

bool cc_engine::Init(std::string& err) {
    HWND hwnd = reinterpret_cast<HWND>(opt.hwnd);
    HMONITOR mon = hwnd ? MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) : reinterpret_cast<HMONITOR>(opt.hmonitor);
    ComPtr<IDXGIAdapter1> adapter = AdapterFor(mon);
    D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    HRESULT hr = D3D11CreateDevice(adapter.Get(), adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE, nullptr, flags, levels, 2,
                                   D3D11_SDK_VERSION, &dev, nullptr, &ctx);
    if (FAILED(hr)) { err = "Couldn't open the graphics card (" + Hex(hr) + ")"; return false; }
    ComPtr<ID3D10Multithread> mt;
    if (SUCCEEDED(ctx.As(&mt))) mt->SetMultithreadProtected(TRUE);
    if (FAILED(dev.As(&vdev)) || FAILED(ctx.As(&vctx))) { err = "The graphics card has no video processor"; return false; }

    // WinRT device + capture item.
    ComPtr<IDXGIDevice> dxgi;
    dev.As(&dxgi);
    winrt::com_ptr<::IInspectable> insp;
    hr = CreateDirect3D11DeviceFromDXGIDevice(dxgi.Get(), insp.put());
    if (FAILED(hr)) { err = "Couldn't share the graphics card with Windows capture (" + Hex(hr) + ")"; return false; }
    rtdev = insp.as<winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>();
    try {
        auto interop = winrt::get_activation_factory<wgc::GraphicsCaptureItem, IGraphicsCaptureItemInterop>();
        if (hwnd)
            winrt::check_hresult(interop->CreateForWindow(hwnd, winrt::guid_of<wgc::GraphicsCaptureItem>(), winrt::put_abi(item)));
        else
            winrt::check_hresult(interop->CreateForMonitor(mon, winrt::guid_of<wgc::GraphicsCaptureItem>(), winrt::put_abi(item)));
    } catch (winrt::hresult_error const& e) {
        err = "Windows won't capture that " + std::string(hwnd ? "window" : "screen") + " (" + Hex(e.code()) + ")";
        return false;
    }
    auto size = item.Size();
    if (size.Width < 16 || size.Height < 16) { err = "The window is too small (minimized?)"; return false; }
    outW = opt.width ? opt.width : static_cast<UINT>(size.Width);
    outH = opt.height ? opt.height : static_cast<UINT>(size.Height);
    outW &= ~1u, outH &= ~1u;

    // Encoder textures.
    for (int i = 0; i < kPool; i++) {
        D3D11_TEXTURE2D_DESC d{};
        d.Width = outW, d.Height = outH, d.MipLevels = 1, d.ArraySize = 1, d.Format = DXGI_FORMAT_NV12;
        d.SampleDesc.Count = 1, d.Usage = D3D11_USAGE_DEFAULT, d.BindFlags = D3D11_BIND_RENDER_TARGET;
        if (FAILED(hr = dev->CreateTexture2D(&d, nullptr, &nv12[i]))) { err = "Out of video memory (" + Hex(hr) + ")"; return false; }
    }

    std::string encErr;
    bool ok = enc.Init(dev.Get(), outW, outH, opt.fps, opt.kbps, opt.max_kbps, static_cast<int>(opt.codec),
                       [this](const uint8_t* d, size_t n, int64_t t, bool key) {
                           encoded++;
                           lastPacket = t;
                           if (opt.on_packet) opt.on_packet(opt.user, d, n, t, key ? 1 : 0);
                       },
                       encErr);
    if (!ok) { err = encErr; return false; }

    try {
        poolSize = size;
        pool = wgc::Direct3D11CaptureFramePool::CreateFreeThreaded(rtdev, wdx::DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size);
        arrived = pool.FrameArrived([this](auto const& sender, auto const&) { OnFrame(sender); });
        session = pool.CreateCaptureSession(item);
        session.IsCursorCaptureEnabled(opt.cursor != 0);
        using winrt::Windows::Foundation::Metadata::ApiInformation;
        if (ApiInformation::IsPropertyPresent(L"Windows.Graphics.Capture.GraphicsCaptureSession", L"IsBorderRequired")) {
            try { session.IsBorderRequired(false); } catch (...) {}
        }
        if (ApiInformation::IsPropertyPresent(L"Windows.Graphics.Capture.GraphicsCaptureSession", L"MinUpdateInterval")) {
            try { session.MinUpdateInterval(winrt::Windows::Foundation::TimeSpan{10'000'000 / std::max<uint32_t>(opt.fps, 1) / 2}); } catch (...) {}
        }
        session.StartCapture();
    } catch (winrt::hresult_error const& e) {
        err = "Windows capture didn't start (" + Hex(e.code()) + ")";
        return false;
    }
    pacer = std::thread([this] { Pace(); });
    return true;
}

void cc_engine::OnFrame(wgc::Direct3D11CaptureFramePool const& sender) {
    if (stop) return;
    try {
        auto frame = sender.TryGetNextFrame();
        if (!frame) return;
        auto cs = frame.ContentSize();
        int64_t t = frame.SystemRelativeTime().count();
        auto access = frame.Surface().as<::Windows::Graphics::DirectX::Direct3D11::IDirect3DDxgiInterfaceAccess>();
        ComPtr<ID3D11Texture2D> tex;
        if (FAILED(access->GetInterface(IID_PPV_ARGS(&tex)))) return;
        D3D11_TEXTURE2D_DESC sd{};
        tex->GetDesc(&sd);
        UINT w = std::min<UINT>(static_cast<UINT>(std::max(cs.Width, 2)), sd.Width) & ~1u;
        UINT h = std::min<UINT>(static_cast<UINT>(std::max(cs.Height, 2)), sd.Height) & ~1u;
        {
            std::lock_guard<std::mutex> lk(gpu);
            if (!latest || latestW != w || latestH != h) {
                D3D11_TEXTURE2D_DESC d{};
                d.Width = w, d.Height = h, d.MipLevels = 1, d.ArraySize = 1, d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
                d.SampleDesc.Count = 1, d.Usage = D3D11_USAGE_DEFAULT, d.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
                latest.Reset();
                if (FAILED(dev->CreateTexture2D(&d, nullptr, &latest))) return;
                latestW = w, latestH = h;
                latestGen++;
            }
            D3D11_BOX box{0, 0, 0, w, h, 1};
            ctx->CopySubresourceRegion(latest.Get(), 0, 0, 0, 0, tex.Get(), 0, &box);
            haveFrame = true;
            newSinceTick = true;
        }
        lastCapture = t;
        captured++;
        // The window changed size: let the pool match it (the output size stays).
        if (cs.Width != poolSize.Width || cs.Height != poolSize.Height) {
            poolSize = cs;
            sender.Recreate(rtdev, wdx::DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, cs);
        }
    } catch (...) {
        // A window closing mid-frame: the next frame (or the app) sorts it out.
    }
}

bool cc_engine::EnsureVp(UINT inW, UINT inH) {
    if (vp && vpInW == inW && vpInH == inH) return true;
    vp.Reset(), vpEnum.Reset(), inView.Reset(), inViewGen = ~0ull;
    for (auto& v : outView) v.Reset();
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC cd{};
    cd.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    cd.InputWidth = inW, cd.InputHeight = inH, cd.OutputWidth = outW, cd.OutputHeight = outH;
    cd.InputFrameRate = {opt.fps, 1}, cd.OutputFrameRate = {opt.fps, 1};
    cd.Usage = D3D11_VIDEO_USAGE_OPTIMAL_SPEED;
    if (FAILED(vdev->CreateVideoProcessorEnumerator(&cd, &vpEnum)) || FAILED(vdev->CreateVideoProcessor(vpEnum.Get(), 0, &vp))) return false;
    for (int i = 0; i < kPool; i++) {
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC od{};
        od.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
        if (FAILED(vdev->CreateVideoProcessorOutputView(nv12[i].Get(), vpEnum.Get(), &od, &outView[i]))) return false;
    }
    // RGB full range in, BT.709 limited range out.
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE in{};
    in.RGB_Range = 0;
    vctx->VideoProcessorSetStreamColorSpace(vp.Get(), 0, &in);
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE out{};
    out.YCbCr_Matrix = 1;
    out.Nominal_Range = D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235;
    vctx->VideoProcessorSetOutputColorSpace(vp.Get(), &out);
    D3D11_VIDEO_COLOR black{};
    black.YCbCr = {0.0625f, 0.5f, 0.5f, 1.0f};
    vctx->VideoProcessorSetOutputBackgroundColor(vp.Get(), TRUE, &black);
    vctx->VideoProcessorSetStreamFrameFormat(vp.Get(), 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
    vctx->VideoProcessorSetStreamAutoProcessingMode(vp.Get(), 0, FALSE);
    // Keep the shape: fit inside the output, black bars if the window's shape changed.
    RECT src{0, 0, static_cast<LONG>(inW), static_cast<LONG>(inH)};
    double s = std::min(static_cast<double>(outW) / inW, static_cast<double>(outH) / inH);
    LONG dw = static_cast<LONG>(inW * s) & ~1, dh = static_cast<LONG>(inH * s) & ~1;
    LONG dx = (static_cast<LONG>(outW) - dw) / 2 & ~1, dy = (static_cast<LONG>(outH) - dh) / 2 & ~1;
    RECT dst{dx, dy, dx + dw, dy + dh};
    vctx->VideoProcessorSetStreamSourceRect(vp.Get(), 0, TRUE, &src);
    vctx->VideoProcessorSetStreamDestRect(vp.Get(), 0, TRUE, &dst);
    RECT full{0, 0, static_cast<LONG>(outW), static_cast<LONG>(outH)};
    vctx->VideoProcessorSetOutputTargetRect(vp.Get(), TRUE, &full);
    vpInW = inW, vpInH = inH;
    return true;
}

void cc_engine::Pace() {
    winrt::init_apartment(winrt::apartment_type::multi_threaded);
    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    HANDLE timer = CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
    const double interval = 1e7 / std::max<uint32_t>(opt.fps, 1);
    const int64_t start = QpcHns();
    uint64_t n = 0;
    while (!stop) {
        int64_t due = start + static_cast<int64_t>(n * interval);
        int64_t now = QpcHns();
        if (due > now) {
            if (timer) {
                LARGE_INTEGER rel;
                rel.QuadPart = -(due - now);
                SetWaitableTimer(timer, &rel, 0, nullptr, nullptr, FALSE);
                WaitForSingleObject(timer, 100);
            } else {
                Sleep(static_cast<DWORD>((due - now) / 10'000));
            }
            continue;
        }
        // Far behind (the PC was stalled): skip ahead rather than catch up in a burst.
        if (now - due > static_cast<int64_t>(interval * 3)) {
            uint64_t skip = static_cast<uint64_t>((now - due) / interval);
            dropped += skip;
            n += skip;
            due = start + static_cast<int64_t>(n * interval);
        }
        if (enc.Failed()) { Fail(enc.Error()); break; }
        Tick(due, static_cast<int64_t>(interval));
        n++;
    }
    if (timer) CloseHandle(timer);
}

void cc_engine::Tick(int64_t t, int64_t dur) {
    if (enc.Pending() >= 3) { dropped++; return; }
    std::lock_guard<std::mutex> lk(gpu);
    if (!haveFrame) return;  // nothing captured yet
    if (!newSinceTick) repeated++;
    newSinceTick = false;
    if (!EnsureVp(latestW, latestH)) { Fail("The graphics card couldn't convert the frames"); return; }
    if (inViewGen != latestGen) {
        inView.Reset();
        D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC iv{};
        iv.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
        if (FAILED(vdev->CreateVideoProcessorInputView(latest.Get(), vpEnum.Get(), &iv, &inView))) { dropped++; return; }
        inViewGen = latestGen;
    }
    int i = next;
    next = (next + 1) % kPool;
    D3D11_VIDEO_PROCESSOR_STREAM stream{};
    stream.Enable = TRUE;
    stream.pInputSurface = inView.Get();
    if (FAILED(vctx->VideoProcessorBlt(vp.Get(), outView[i].Get(), 0, 1, &stream))) { dropped++; return; }
    if (!enc.Submit(nv12[i].Get(), t, dur)) dropped++;
    if (opt.tap_every && opt.on_tap && frameNo % opt.tap_every == 0) Tap(t);
    frameNo++;
}

// Killfeed corner, like the classic recorder's tap: width min(w, tapW*h/1080),
// height tapH*h/1080, top 40*h/1080, right-aligned; scaled to tapW x tapH, BGR.
void cc_engine::Tap(int64_t t) {
    const UINT W = latestW, H = latestH;
    UINT cw = std::min<UINT>(W, opt.tap_w * H / 1080), ch = std::min<UINT>(H, opt.tap_h * H / 1080);
    UINT cx = W - cw, cy = std::min<UINT>(H - ch, 40 * H / 1080);
    if (cw < 8 || ch < 8) return;
    // Read the copy made one tap ago (finished by now), then start this one.
    int prev = 1 - tapSlot;
    if (tapPending[prev] && tapStage[prev]) {
        D3D11_MAPPED_SUBRESOURCE m{};
        if (SUCCEEDED(ctx->Map(tapStage[prev].Get(), 0, D3D11_MAP_READ, 0, &m))) {
            const UINT sw = tapW[prev], sh = tapH[prev], ow = opt.tap_w, oh = opt.tap_h;
            tapOut.resize(static_cast<size_t>(ow) * oh * 3);
            for (UINT y = 0; y < oh; y++) {
                double fy = (y + 0.5) * sh / oh - 0.5;
                int y0 = std::clamp(static_cast<int>(fy), 0, static_cast<int>(sh) - 1), y1 = std::min<int>(y0 + 1, sh - 1);
                double wy = std::clamp(fy - y0, 0.0, 1.0);
                const uint8_t* r0 = static_cast<const uint8_t*>(m.pData) + static_cast<size_t>(y0) * m.RowPitch;
                const uint8_t* r1 = static_cast<const uint8_t*>(m.pData) + static_cast<size_t>(y1) * m.RowPitch;
                uint8_t* o = tapOut.data() + static_cast<size_t>(y) * ow * 3;
                for (UINT x = 0; x < ow; x++) {
                    double fx = (x + 0.5) * sw / ow - 0.5;
                    int x0 = std::clamp(static_cast<int>(fx), 0, static_cast<int>(sw) - 1), x1 = std::min<int>(x0 + 1, sw - 1);
                    double wx = std::clamp(fx - x0, 0.0, 1.0);
                    for (int c = 0; c < 3; c++) {
                        double a = r0[x0 * 4 + c] * (1 - wx) + r0[x1 * 4 + c] * wx;
                        double b = r1[x0 * 4 + c] * (1 - wx) + r1[x1 * 4 + c] * wx;
                        o[x * 3 + c] = static_cast<uint8_t>(a * (1 - wy) + b * wy + 0.5);
                    }
                }
            }
            ctx->Unmap(tapStage[prev].Get(), 0);
            opt.on_tap(opt.user, tapOut.data(), ow, oh, tapTime[prev]);
        }
        tapPending[prev] = false;
    }
    int s = tapSlot;
    if (!tapStage[s] || tapW[s] != cw || tapH[s] != ch) {
        D3D11_TEXTURE2D_DESC d{};
        d.Width = cw, d.Height = ch, d.MipLevels = 1, d.ArraySize = 1, d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        d.SampleDesc.Count = 1, d.Usage = D3D11_USAGE_STAGING, d.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        tapStage[s].Reset();
        if (FAILED(dev->CreateTexture2D(&d, nullptr, &tapStage[s]))) return;
        tapW[s] = cw, tapH[s] = ch;
    }
    D3D11_BOX box{cx, cy, 0, cx + cw, cy + ch, 1};
    ctx->CopySubresourceRegion(tapStage[s].Get(), 0, 0, 0, 0, latest.Get(), 0, &box);
    tapTime[s] = t;
    tapPending[s] = true;
    tapSlot = 1 - s;
}

void cc_engine::Shutdown() {
    stop = true;
    if (pacer.joinable()) pacer.join();
    try {
        if (pool) { pool.FrameArrived(arrived); }
        if (session) session.Close();
        if (pool) pool.Close();
    } catch (...) {
    }
    session = nullptr, pool = nullptr, item = nullptr;
    enc.Stop();
}

extern "C" cc_engine* cc_start(const cc_options* o, char* err, size_t errlen) {
    if (!o || !o->fps || !o->kbps || (!o->hwnd && !o->hmonitor)) { Copy(err, errlen, "bad options"); return nullptr; }
    try {
        winrt::init_apartment(winrt::apartment_type::multi_threaded);
    } catch (...) {
        // Already initialised on this thread (any model is fine for the free-threaded pool).
    }
    MFStartup(MF_VERSION, MFSTARTUP_LITE);
    auto* e = new cc_engine();
    e->opt = *o;
    std::string why;
    bool ok = false;
    try {
        ok = e->Init(why);
    } catch (winrt::hresult_error const& x) {
        why = "Capture failed (" + Hex(x.code()) + ")";
    } catch (...) {
        why = "Capture failed";
    }
    if (!ok) {
        e->Shutdown();
        delete e;
        MFShutdown();
        Copy(err, errlen, why);
        return nullptr;
    }
    return e;
}

extern "C" void cc_get_stats(cc_engine* e, cc_stats* s) {
    if (!e || !s) return;
    memset(s, 0, sizeof *s);
    s->width = e->outW, s->height = e->outH;
    s->src_width = e->latestW, s->src_height = e->latestH;
    s->captured = e->captured, s->encoded = e->encoded, s->repeated = e->repeated, s->dropped = e->dropped;
    s->last_capture_hns = e->lastCapture, s->last_packet_hns = e->lastPacket;
    bool f = e->failed || e->enc.Failed();
    s->failed = f ? 1 : 0;
    Copy(s->encoder, sizeof s->encoder, e->enc.Name());
    std::lock_guard<std::mutex> lk(e->errMu);
    Copy(s->error, sizeof s->error, !e->error.empty() ? e->error : e->enc.Error());
}

extern "C" void cc_stop(cc_engine* e) {
    if (!e) return;
    e->Shutdown();
    delete e;
    MFShutdown();
}
