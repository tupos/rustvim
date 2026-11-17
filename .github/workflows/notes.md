```
${NVIM_VERSION}
```

## Release notes

- [Source commit](https://github.com/${GH_REPO}/commit/${NVIM_COMMIT})
- [News](https://github.com/${GH_REPO}/blob/${NVIM_COMMIT}/runtime/doc/news.txt) (`:help news`)

## Install

### Windows

#### Zip

1. Download **nvim-win64.zip** (or **nvim-win-arm64.zip** for ARM)
2. Extract the zip
3. Run `nvim.exe` in your terminal

#### MSI

1. Download **nvim-win64.msi** (or **nvim-win-arm64.msi** for ARM)
2. Run the MSI
3. Run `nvim.exe` in your terminal

If a required runtime DLL is missing, install the [Microsoft Visual C++ Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist).

### macOS (x86_64)

1. Download **nvim-macos-x86_64.tar.gz**
2. Run `xattr -c ./nvim-macos-x86_64.tar.gz` (to avoid "unknown developer" warning)
3. Extract: `tar xzvf nvim-macos-x86_64.tar.gz`
4. Run `./nvim-macos-x86_64/bin/nvim`

### macOS (arm64)

1. Download **nvim-macos-arm64.tar.gz**
2. Run `xattr -c ./nvim-macos-arm64.tar.gz` (to avoid "unknown developer" warning)
3. Extract: `tar xzvf nvim-macos-arm64.tar.gz`
4. Run `./nvim-macos-arm64/bin/nvim`

### Linux (x86_64)

The Linux packages are built on Ubuntu 22.04. For older glibc systems, build Rustvim from source.

#### AppImage

1. Download **nvim-linux-x86_64.appimage**
2. Run `chmod u+x nvim-linux-x86_64.appimage && ./nvim-linux-x86_64.appimage`
   - If your system does not have FUSE you can [extract the appimage](https://github.com/AppImage/AppImageKit/wiki/FUSE#type-2-appimage):
     ```bash
     ./nvim-linux-x86_64.appimage --appimage-extract
     ./squashfs-root/usr/bin/nvim
     ```

#### Tarball

1. Download **nvim-linux-x86_64.tar.gz**
2. Extract: `tar xzvf nvim-linux-x86_64.tar.gz`
3. Run `./nvim-linux-x86_64/bin/nvim`

### Linux (arm64)

#### AppImage

1. Download **nvim-linux-arm64.appimage**
2. Run `chmod u+x nvim-linux-arm64.appimage && ./nvim-linux-arm64.appimage`
   - If your system does not have FUSE you can [extract the appimage](https://github.com/AppImage/AppImageKit/wiki/FUSE#type-2-appimage):
     ```bash
     ./nvim-linux-arm64.appimage --appimage-extract
     ./squashfs-root/usr/bin/nvim
     ```

#### Tarball

1. Download **nvim-linux-arm64.tar.gz**
2. Extract: `tar xzvf nvim-linux-arm64.tar.gz`
3. Run `./nvim-linux-arm64/bin/nvim`

### Browser (WASM)

1. Download **nvim-wasm-emscripten.zip** and extract it.
2. Run `python3 serve.py` in the extracted directory.
3. Open http://localhost:8002 in your browser.

The included server sets the cross-origin isolation headers required by WASM threads.

### Other

- [Build from source](https://github.com/${GH_REPO}/blob/${NVIM_COMMIT}/BUILD.md)
