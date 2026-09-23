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

      # The OVMF build the malware-analysis-x64 profile imports as its
      # firmware_code and firmware_vars. Stock OVMF gives itself away in two
      # places QEMU cannot reach, so both are changed at build time:
      #
      # * The BGRT comes from BootGraphicsResourceTableDxe, not from QEMU, and
      #   reads its ACPI header from build-time PCDs whose defaults are
      #   INTEL/EDK2 with an empty creator. Every table QEMU builds carries the
      #   profile's analysis.acpi identity, so a stock BGRT would be the one
      #   table that disagrees. The identity is read from the profile itself, so
      #   the two cannot drift apart.
      # * OVMF measures PEIFV and DXEFV into PCR 0 with their base and length,
      #   and the stock pairs (0x830000/0xD0000, 0x900000/0xE80000) are a
      #   published signature. Either pair matching is enough, so both move;
      #   DXEFV ends exactly at the end of MEMFD, which grows by the same shift.
      #
      # The logo is swapped as well, since EDK II's 193x58 bitmap is published
      # through the same BGRT. Override bootLogo or fvShift with .override.
      mkAnalysisOvmf =
        pkgs:
        pkgs.lib.makeOverridable (
          {
            profile ? builtins.fromJSON (builtins.readFile ./catalog/profiles/malware-analysis-x64.json),
            bootLogo ? ./assets/analysis/neutral-boot-logo.bmp,
            fvShift ? 65536,
          }:
          let
            acpi = profile.analysis.acpi or { };
            str = name: toString (acpi.${name} or "");
          in
          assert fvShift > 0 && pkgs.lib.mod fvShift 65536 == 0;
          (pkgs.OVMF.override {
            secureBoot = true;
            tpmSupport = true;
            msVarsTemplate = true;
          }).overrideAttrs
            (oldAttrs: {
              pname = "${oldAttrs.pname}-analysis";
              # Passed as derivation attributes so the builder sees them as
              # environment variables; the packing is easier in shell than in Nix.
              acpiOemId = str "oem_id";
              acpiOemTableId = str "oem_table_id";
              acpiOemRevision = str "oem_revision";
              acpiCreatorId = str "creator_id";
              acpiCreatorRevision = str "creator_revision";
              inherit fvShift;
              postPatch = (oldAttrs.postPatch or "") + ''
                cp -v ${bootLogo} MdeModulePkg/Logo/Logo.bmp

                # OemId is copied as six bytes and OemTableId/CreatorId are
                # packed little-endian into a UINT64/UINT32, so each value is
                # padded to the width the ACPI header reserves for it.
                pcdPackLe() {
                  local hex
                  hex=$(printf '%-*.*s' "$2" "$2" "$1" | od -An -v -tx1 | tr -d ' \n')
                  printf '0x%s' "$(echo "$hex" | fold -w2 | tac | tr -d '\n' | tr 'a-f' 'A-F')"
                }
                {
                  echo
                  echo '# Added by machineemu: BGRT identity, see docs/analysis-firmware.md.'
                  echo '[PcdsFixedAtBuild]'
                  if [ -n "$acpiOemId" ]; then
                    printf '  gEfiMdeModulePkgTokenSpaceGuid.PcdAcpiDefaultOemId|"%-6.6s"|VOID*|7\n' "$acpiOemId"
                  fi
                  if [ -n "$acpiOemTableId" ]; then
                    printf '  gEfiMdeModulePkgTokenSpaceGuid.PcdAcpiDefaultOemTableId|%s\n' \
                      "$(pcdPackLe "$acpiOemTableId" 8)"
                  fi
                  if [ -n "$acpiOemRevision" ]; then
                    printf '  gEfiMdeModulePkgTokenSpaceGuid.PcdAcpiDefaultOemRevision|%s\n' "$acpiOemRevision"
                  fi
                  if [ -n "$acpiCreatorId" ]; then
                    printf '  gEfiMdeModulePkgTokenSpaceGuid.PcdAcpiDefaultCreatorId|%s\n' \
                      "$(pcdPackLe "$acpiCreatorId" 4)"
                  fi
                  if [ -n "$acpiCreatorRevision" ]; then
                    printf '  gEfiMdeModulePkgTokenSpaceGuid.PcdAcpiDefaultCreatorRevision|%s\n' \
                      "$acpiCreatorRevision"
                  fi
                } >> OvmfPkg/OvmfPkgX64.dsc

                memfd=OvmfPkg/Include/Fdf/MemFd.fdf.inc
                shift=$fvShift
                substituteInPlace $memfd \
                  --replace-fail "Size          = 0xF80000" \
                                 "$(printf 'Size          = 0x%X' $((0xF80000 + shift)))" \
                  --replace-fail "NumBlocks     = 0xF8" \
                                 "$(printf 'NumBlocks     = 0x%X' $(((0xF80000 + shift) / 65536)))" \
                  --replace-fail "0x020000|0x10000" \
                                 "$(printf '0x020000|0x%X' $((0x10000 + shift)))" \
                  --replace-fail "0x030000|0x0D0000" \
                                 "$(printf '0x%06X|0x0D0000' $((0x030000 + shift)))" \
                  --replace-fail "0x100000|0xE80000" \
                                 "$(printf '0x%06X|0xE80000' $((0x100000 + shift)))"
                echo "shifted OVMF firmware volumes by $(printf '0x%X' $shift)"
              '';
            })
        ) { };
    in
    {
      # x86_64 only: the profile is an x86_64 guest and the PCD and MEMFD
      # edits are to OvmfPkgX64.
      packages.x86_64-linux.analysis-ovmf = mkAnalysisOvmf (import nixpkgs { system = "x86_64-linux"; });

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };

          # machineemu-core builds rusqlite with the bundled SQLite, so the
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

          # The viewer decodes H.264 with GStreamer. display-stream encodes
          # with GStreamer and links EGL, GLES, and GBM for DMABUF capture.
          viewerLibs = with pkgs; [
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            gst_all_1.gst-plugins-bad
            gst_all_1.gst-libav
            libglvnd
            libgbm
            libdrm
            libva-utils
          ];
          windowLibs = with pkgs; [
            wayland
            libxkbcommon
            libx11
            libxcursor
            libxrandr
            libxi
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

            buildInputs = viewerLibs;

            env = {
              PYTHONDONTWRITEBYTECODE = "1";
              # The planner's helper path: --swtpm and helpers.swtpm in
              # machineemu.yaml override it, PATH resolution is the fallback.
              MACHINEEMU_SWTPM = "${pkgs.swtpm}/bin/swtpm";
              # The bridge helper has to be privileged, so it cannot come from
              # the store: NixOS publishes the wrapped copy here, and
              # security.wrappers is what grants it cap_net_admin.
              MACHINEEMU_BRIDGE_HELPER = "/run/wrappers/bin/qemu-bridge-helper";
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath windowLibs;
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
