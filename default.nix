{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
  cmake,
  installShellFiles,
  writableTmpDirAsHomeHook,

  # deps for audio backends
  alsa-lib,
  libpulseaudio,
  portaudio,
  libjack2,
  SDL2,
  gst_all_1,
  dbus,
  fontconfig,
  libsixel,
  autoconf,
  automake,
  libtool,

  # build options
  withStreaming ? true,
  withDaemon ? true,
  withAudioBackend ? "rodio", # alsa, pulseaudio, rodio, portaudio, jackaudio, rodiojack, sdl, gstreamer
  withMediaControl ? true,
  withImage ? true,
  withNotify ? true,
  withSixel ? true,
  withFuzzy ? true,
  withQuickJs ? true,
  stdenv,
  makeBinaryWrapper,
}:

assert lib.assertOneOf "withAudioBackend" withAudioBackend [
  ""
  "alsa"
  "pulseaudio"
  "rodio"
  "portaudio"
  "jackaudio"
  "rodiojack"
  "sdl"
  "gstreamer"
];

rustPlatform.buildRustPackage rec {
  pname = "unified-player";
  version =
    let
      toml = builtins.fromTOML (builtins.readFile ./unified-player/Cargo.toml);
    in
    toml.package.version;

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    # Keep git-sourced crates reproducible for Nix's Cargo vendoring.
    outputHashes = {
      "rquickjs-0.12.2" = "sha256-Waq7MkGvcCRbHW30a++6FZARJnJ4R9a8GYnGL2wbrAE=";
      "rquickjs-core-0.12.2" = "sha256-Waq7MkGvcCRbHW30a++6FZARJnJ4R9a8GYnGL2wbrAE=";
      "rquickjs-sys-0.12.2" = "sha256-Waq7MkGvcCRbHW30a++6FZARJnJ4R9a8GYnGL2wbrAE=";
    };
  };

  nativeBuildInputs = [
    pkg-config
    cmake
    rustPlatform.bindgenHook
    installShellFiles
    autoconf
    automake
    libtool
    # Tries to access $HOME when installing shell files, and on Darwin
    writableTmpDirAsHomeHook
  ]
  ++ lib.optionals stdenv.hostPlatform.isDarwin [
    makeBinaryWrapper
  ];

  buildInputs = [
    openssl
    dbus
    fontconfig
  ]
  ++ lib.optionals withSixel [ libsixel ]
  ++ lib.optionals (withAudioBackend == "alsa") [ alsa-lib ]
  ++ lib.optionals (withAudioBackend == "pulseaudio") [ libpulseaudio ]
  ++ lib.optionals (withAudioBackend == "rodio" && stdenv.hostPlatform.isLinux) [ alsa-lib ]
  ++ lib.optionals (withAudioBackend == "portaudio") [ portaudio ]
  ++ lib.optionals (withAudioBackend == "jackaudio") [ libjack2 ]
  ++ lib.optionals (withAudioBackend == "rodiojack") [
    alsa-lib
    libjack2
  ]
  ++ lib.optionals (withAudioBackend == "sdl") [ SDL2 ]
  ++ lib.optionals (withAudioBackend == "gstreamer") [
    gst_all_1.gstreamer
    gst_all_1.gst-devtools
    gst_all_1.gst-plugins-base
    gst_all_1.gst-plugins-good
  ];

  buildNoDefaultFeatures = true;

  buildFeatures =
    [ ]
    ++ lib.optionals (withAudioBackend != "") [ "${withAudioBackend}-backend" ]
    ++ lib.optionals withMediaControl [ "media-control" ]
    ++ lib.optionals withImage [ "image" ]
    ++ lib.optionals withDaemon [ "daemon" ]
    ++ lib.optionals withNotify [ "notify" ]
    ++ lib.optionals withStreaming [ "streaming" ]
    ++ lib.optionals withSixel [ "sixel" ]
    ++ lib.optionals withFuzzy [ "fzf" ]
    ++ lib.optionals withQuickJs [ "youtube-quickjs" ];

  postInstall =
    let
      inherit (lib.strings) optionalString;
    in
    # sixel-sys is dynamically linked to libsixel
    optionalString (stdenv.hostPlatform.isDarwin && withSixel) ''
      wrapProgram $out/bin/unified-player \
        --prefix DYLD_LIBRARY_PATH : "${lib.makeLibraryPath [ libsixel ]}"
    ''
    + optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
      installShellCompletion --cmd unified-player \
        --bash <($out/bin/unified-player generate bash) \
        --fish <($out/bin/unified-player generate fish) \
         --zsh <($out/bin/unified-player generate zsh)
    '';

  meta = {
    description = "Unified Player, a provider-neutral terminal music player";
    homepage = "https://github.com/bababoyy/unified-player";
    changelog = "https://github.com/bababoyy/unified-player/releases";
    mainProgram = "unified-player";
    license = lib.licenses.mit;
  };
}
