# Cross-compilation for the release artifacts.
#
# The aarch64-linux musl tarball is cross-built from an x86_64 GitHub runner. rustc
# links x86_64 musl unaided; a foreign architecture needs a linker.
#
# The body is guarded to x86_64-linux because import-tree imports every module on every
# configured system. Without the guard, an aarch64-linux devshell would try to carry an
# x86_64 cross toolchain it does not need.
#
# If a dependency compiles C, a linker is not enough: the cc crate also needs
# CC_x86_64_unknown_linux_musl and CC_aarch64_unknown_linux_musl, underscored because
# devshell exports them. diskfoot's dependencies are pure Rust today.
{ ... }:
{
  perSystem =
    {
      system,
      pkgs,
      lib,
      ...
    }:
    let
      aarch64-cc = pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc;
      aarch64-linker = "${aarch64-cc}/bin/aarch64-unknown-linux-musl-cc";
    in
    lib.optionalAttrs (system == "x86_64-linux") {
      devshells.default = {
        packages = [ aarch64-cc ];

        env = [
          {
            name = "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER";
            value = aarch64-linker;
          }
        ];
      };

      # Exposed so a workflow or a debugging session can resolve the linker without
      # entering the devshell.
      packages.aarch64-musl-cc = aarch64-cc;
    };
}
