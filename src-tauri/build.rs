fn main() {
    // clipcore: the native recording engine (C++, see native/clipcore.h).
    #[cfg(windows)]
    {
        let files = ["native/capture.cpp", "native/mfenc.cpp", "native/aac.cpp"];
        for f in files.iter().chain(["native/clipcore.h", "native/mfenc.h"].iter()) {
            println!("cargo:rerun-if-changed={f}");
        }
        cc::Build::new()
            .cpp(true)
            .files(files)
            .std("c++20")
            .flag("/EHsc")
            .flag("/permissive-")
            .flag("/utf-8")
            .define("WIN32_LEAN_AND_MEAN", None)
            .define("NOMINMAX", None)
            .warnings(false)
            .compile("clipcore");
        for lib in ["d3d11", "dxgi", "mfplat", "mfuuid", "mf", "ole32", "oleaut32", "windowsapp", "strmiids", "wmcodecdspuuid"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
    tauri_build::build()
}
