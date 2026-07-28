// Shared Windows resource + manifest embedding for the proto-* build scripts.
//
// This is not a crate: each `build.rs` pulls it in with
// `include!("../build-common.rs")`, so it rides on that build.rs's own
// `[target.'cfg(windows)'.build-dependencies] winresource` entry instead of needing a
// workspace member (and its own Cargo.toml/publish surface) of its own.
//
// Guarded twice, deliberately:
//   - `CARGO_CFG_TARGET_OS` is the signal that actually decides whether to embed
//     anything: it reflects the OS the binary being produced targets, and is what
//     keeps Linux builds untouched.
//   - `#[cfg(windows)]` on `embed_windows_resource_impl` is required for build.rs to
//     *compile* at all on a non-Windows host: `winresource` is only declared as a
//     build-dependency under `[target.'cfg(windows)'.build-dependencies]`, so an
//     unconditional reference to it would fail to resolve when that table doesn't
//     apply.
// CI never crosses the two (each target OS builds natively on a matching runner), so
// there the resource is always embedded. A local mingw cross-build (Linux host ->
// Windows target) takes the #[cfg(not(windows))] no-op instead -- see its comment.
// `embed_windows_resource` keeps a real (non-tail) `return` on the non-Windows path
// so clippy's `needless_return` doesn't fire when the `#[cfg(windows)]` impl is
// compiled away.

/// Embeds a version resource (product name, file description, company, copyright,
/// file/product version) and an application manifest (execution level, long-path
/// awareness) into the binary currently being built. No-op unless the build target is
/// Windows. Version numbers are not passed in: `winresource::WindowsResource::new()`
/// reads them from `CARGO_PKG_VERSION`, i.e. the crate's own `Cargo.toml`, so there is
/// exactly one place the version is written.
fn embed_windows_resource(product_name: &str, file_description: &str, execution_level: &str) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    embed_windows_resource_impl(product_name, file_description, execution_level);
}

#[cfg(windows)]
fn embed_windows_resource_impl(product_name: &str, file_description: &str, execution_level: &str) {
    let slug = product_name.to_lowercase().replace(' ', "-");
    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="dev.sagasu.{slug}" type="win32"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="{execution_level}" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#,
        slug = slug,
        execution_level = execution_level,
    );

    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", product_name);
    res.set("FileDescription", file_description);
    res.set("CompanyName", "sagasu project");
    res.set("LegalCopyright", "Copyright (c) sagasu project contributors");
    // FileVersion / ProductVersion already default to CARGO_PKG_VERSION (see above);
    // setting them again here would just be the hand-typed second copy outcome 2
    // asks us to avoid.
    res.set_manifest(&manifest);
    if let Err(e) = res.compile() {
        panic!("failed to embed Windows resource/manifest for {product_name}: {e}");
    }
}

// Linux ホストから Windows ターゲットへのクロスビルド(README の「手元動作確認の簡便法」
// である mingw 経路)ではここに到達する: CARGO_CFG_TARGET_OS はターゲット(windows)を見て
// impl へ進むが、build.rs 自体はホスト(linux)向けにコンパイルされるので、この
// #[cfg(not(windows))] 側が選ばれる。winresource を呼ぶことはできない
// (依存の cfg はターゲット基準・コードの cfg はホスト基準で、ネイティブ Linux ビルド
// では依存自体が存在しないため参照した時点でコンパイルが壊れる)。
// パニックではなく警告付き no-op にする: 従来の mingw クロスビルドと同じ
// 「リソース無しのバイナリが出る」挙動を保つ。配布物はCI(Windowsネイティブ)が作る。
#[cfg(not(windows))]
fn embed_windows_resource_impl(product_name: &str, _file_description: &str, _execution_level: &str) {
    println!(
        "cargo:warning={product_name}: cross-compiling from a non-Windows host; \
         version resource / manifest NOT embedded (published binaries come from the \
         Windows CI build)"
    );
}
