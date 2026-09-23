{
  description = "aa-player";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        toolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
          "rust-analyzer"
        ];

        rustPlatform = pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };

        gstPlugins = with pkgs.gst_all_1; [
          gstreamer
          gst-plugins-base
          gst-plugins-good
          gst-plugins-bad
          gst-plugins-ugly
          gst-libav
        ];

        nativeBuildInputs = with pkgs; [
          pkg-config
          protobuf # prost-build
          cmake # libsamplerate-sys
          makeWrapper
        ];

        buildInputs = [
          pkgs.alsa-lib
          pkgs.udev
        ]
        ++ gstPlugins;
      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "aa_player";
          version = "0.1.0";
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "rustls-webpki-0.103.2" = "sha256-be4ivb03sSvcFV3RtYsgC2rkqju0XB/WxUUuWg58r8s=";
            };
          };

          inherit nativeBuildInputs buildInputs;

          PROTOC = "${pkgs.protobuf}/bin/protoc";
          # bundled libsamplerate (libsamplerate-sys) declares cmake_minimum_required < 3.5
          CMAKE_POLICY_VERSION_MINIMUM = "3.5";

          postInstall = ''
            wrapProgram $out/bin/aa_player \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "$GST_PLUGIN_SYSTEM_PATH_1_0"
          '';
        };

        devShells.default = pkgs.mkShell {
          vscodeExtensions = [
            "rust-lang.rust-analyzer"
            "jnoortheen.nix-ide"
          ];
          packages = with pkgs;[
            bashInteractive
            toolchain
            gst_all_1.gstreamer.bin # gst-launch-1.0, gst-inspect-1.0
          ]
          ++ nativeBuildInputs;
          inherit buildInputs;

          PROTOC = "${pkgs.protobuf}/bin/protoc";
          # bundled libsamplerate (libsamplerate-sys) declares cmake_minimum_required < 3.5
          CMAKE_POLICY_VERSION_MINIMUM = "3.5";
          RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
          # glib is only propagated by gstreamer, so list it explicitly
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (buildInputs ++ [ pkgs.glib ]);
        };
      }
    );
}
