{
  description = "MachineEmu planner, daemon and catalog: the Rust toolchain plus the host programs a run needs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      # QEMU, swtpm and the KVM accelerator a run expects are Linux-only, so
      # Darwin is not offered. The engine bundles themselves are built in
      # ../qemu, which pins its own toolchain.
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };

          # machineemu-runtime builds rusqlite with the bundled SQLite, so the
          # shell needs a C compiler even though no crate is a C binding.
          rustPackages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            gcc
            pkg-config
          ];

          # Programs a launch plan starts or shells out to. A missing one is not
          # a build failure but a run that stops at the first instance: the
          # planner names `swtpm` for any profile with a `tpm` section, the
          # runtime calls `qemu-img` for instance overlays and `setsid` to
          # detach QEMU, and the Python image tooling calls `mke2fs`, `openssl`
          # and `swtpm_setup`.
          hostHelpers = with pkgs; [
            qemu
            swtpm
            e2fsprogs
            openssl
            util-linux
            iproute2
          ];

          # Firmware is consumed as a content-addressed asset, not from PATH:
          # the profile pins ovmf-code and ovmf-vars by digest, so these paths
          # are what an import reads from. aarch64 ships the same images under
          # their AAVMF names.
          firmware = pkgs.OVMFFull.fd;
          firmwarePrefix = if pkgs.stdenv.hostPlatform.isx86_64 then "OVMF" else "AAVMF";

          devShell = pkgs.mkShellNoCC {
            packages =
              with pkgs;
              [
                git
                ripgrep
                jq
                curl
                sqlite
                nixfmt
                uv
                (python3.withPackages (ps: [
                  ps.pytest
                  ps.pyyaml
                ]))
                # Seeds are built outside this repo (../../vmmanager-sh), but a
                # NoCloud ISO is often wanted beside a run.
                cloud-utils
                xorriso
              ]
              ++ rustPackages
              ++ hostHelpers;

            env = {
              PYTHONDONTWRITEBYTECODE = "1";
              # The planner's helper path: --swtpm and helpers.swtpm in
              # machineemu.yaml override it, PATH resolution is the fallback.
              MACHINEEMU_SWTPM = "${pkgs.swtpm}/bin/swtpm";
              # The bridge helper has to be privileged, so it cannot come from
              # the store: NixOS publishes the wrapped copy here, and
              # security.wrappers is what grants it cap_net_admin.
              MACHINEEMU_BRIDGE_HELPER = "/run/wrappers/bin/qemu-bridge-helper";
              OVMF_CODE = "${firmware}/FV/${firmwarePrefix}_CODE.fd";
              OVMF_VARS = "${firmware}/FV/${firmwarePrefix}_VARS.fd";
            };

            shellHook = ''
              # The Secure Boot images carry Microsoft's keys already enrolled;
              # a profile that declares secure firmware wants these rather than
              # the pristine pair above. aarch64 publishes no .ms variant.
              if [ -f "${firmware}/FV/${firmwarePrefix}_CODE.ms.fd" ]; then
                export OVMF_CODE_MS="${firmware}/FV/${firmwarePrefix}_CODE.ms.fd"
                export OVMF_VARS_MS="${firmware}/FV/${firmwarePrefix}_VARS.ms.fd"
              fi
              echo "qemu=$(qemu-img --version | head -1 | cut -d' ' -f3) swtpm=$(swtpm --version | head -1 | sed 's/.*version //;s/,.*//')"
              echo "MACHINEEMU_SWTPM=$MACHINEEMU_SWTPM"
              echo "OVMF_CODE=$OVMF_CODE"
              echo "Daemon:  cargo run --bin machineemu-daemon -- --workspace ./machineemu-workspace --unix-socket ./machineemu-workspace/control.sock"
              echo "Run:     cargo run --bin machineemu -- run <profile> <instance>"
            '';
          };
        in
        {
          machineemu = devShell;
          default = devShell;
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
