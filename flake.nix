{
  description = "Turns a HomePod into a PipeWire audio output via AirPlay 2";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Builds the dependencies in their own derivation, so a change to src/
    # recompiles only this crate.
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          # Shared by the dependency build and the crate's, so they agree on
          # features and toolchain inputs. No version here: it changes every
          # commit and would invalidate the dependency build.
          commonArgs = {
            pname = "homepod-sink";
            # Only what cargo reads, so editing this file, the README or the
            # packaging doesn't rebuild anything.
            src = nixpkgs.lib.fileset.toSource {
              root = ./.;
              fileset = nixpkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
              ];
            };
            strictDeps = true;
            # CI runs the tests on every push; here they'd mean compiling the
            # crate a second time.
            doCheck = false;

            # pipewire-sys runs bindgen against the PipeWire headers.
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.rustPlatform.bindgenHook
            ];
            buildInputs = [ pkgs.pipewire ];
          };
        in
        {
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.homepod-sink;
          homepod-sink = craneLib.buildPackage (
            commonArgs
            // {
              # Rebuilt only when Cargo.toml or Cargo.lock change.
              cargoArtifacts = craneLib.buildDepsOnly commonArgs;
              # Same scheme as the Arch package: r<commit count>.<short hash>.
              version =
                if self ? revCount then "r${toString self.revCount}.${self.shortRev}" else "dirty";

              meta = {
                description = "Turns a HomePod into a PipeWire audio output via AirPlay 2";
                homepage = "https://github.com/filedesless/homepod-sink";
                license = nixpkgs.lib.licenses.gpl3Plus;
                mainProgram = "homepod-sink";
                platforms = systems;
              };
            }
          );
        }
      );

      # A systemd user service for every user, like the Arch package's.
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.services.homepod-sink;
        in
        {
          options.services.homepod-sink = {
            enable = lib.mkEnableOption "homepod-sink, a PipeWire sink for every AirPlay 2 device";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "The homepod-sink package to run.";
            };
            sampleRate = lib.mkOption {
              type = lib.types.ints.positive;
              default = 48000;
              description = "Sample rate of the virtual sinks; should match PipeWire's clock.rate.";
            };
            renderDelayMs = lib.mkOption {
              type = lib.types.ints.between 0 2000;
              default = 250;
              description = "How far ahead the HomePod is told to play audio, in ms.";
            };
            udpPortBase = lib.mkOption {
              type = lib.types.port;
              default = 6001;
              description = "First local UDP port the HomePod sends to (it and the 15 above). 0 picks random ports.";
            };
            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Open mDNS (UDP 5353) for discovery, and udpPortBase and the 15 ports above it for the HomePod's timing and retransmit requests.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.user.services.homepod-sink = {
              description = "HomePod AirPlay audio sink (PipeWire virtual output)";
              # PipeWire must be up first so the virtual sink nodes can be created.
              after = [
                "pipewire.service"
                "pipewire-session-manager.service"
              ];
              wants = [
                "pipewire.service"
                "pipewire-session-manager.service"
              ];
              wantedBy = [ "default.target" ];
              serviceConfig = {
                ExecStart = lib.escapeShellArgs [
                  (lib.getExe cfg.package)
                  "--sample-rate"
                  cfg.sampleRate
                  "--render-delay-ms"
                  cfg.renderDelayMs
                  "--udp-port-base"
                  cfg.udpPortBase
                ];
                Restart = "on-failure";
                RestartSec = 5;
              };
            };

            networking.firewall = lib.mkIf cfg.openFirewall {
              # mDNS replies are unsolicited multicast, which conntrack drops.
              allowedUDPPorts = [ 5353 ];
              allowedUDPPortRanges = lib.mkIf (cfg.udpPortBase != 0) [
                {
                  from = cfg.udpPortBase;
                  to = cfg.udpPortBase + 15;
                }
              ];
            };
          };
        };
    };
}
