# Prebuilt release binaries, for consumers who want diskfoot without a Rust toolchain
# entering their build closure.
#
# The pinned version deliberately trails Cargo.toml: it names the last tag whose
# artifacts are published, not whatever the working tree happens to claim. This
# repository has not tagged a release yet, so both hashes are lib.fakeHash and this
# package must not be built. After the first tag's Release workflow finishes, run
# `just release-hashes vX.Y.Z` and paste the two SRI values here, then bump `version`.
#
# The hashes are of the tarballs the release publishes, so they can be checked against
# the .sha256 assets rather than taken on faith. Systems absent from `hashes` do not get
# the package; diskfoot is Linux-only.
#
# No makeWrapper: diskfoot optionally shells out to `btrfs`, `zfs` and `xfs_quota` for
# its fast paths, but those are host system tools. A missing one falls back to the
# generic walk, so bundling them into the closure would only add weight.
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
      release = {
        version = "0.1.0";
        hashes = {
          "x86_64-linux" = lib.fakeHash;
          "aarch64-linux" = lib.fakeHash;
        };
      };

      triple = {
        "x86_64-linux" = "x86_64-unknown-linux-musl";
        "aarch64-linux" = "aarch64-unknown-linux-musl";
      };

      tag = "v${release.version}";

      tarball = pkgs.fetchurl {
        url = "https://github.com/sshine/diskfoot/releases/download/${tag}/diskfoot-${tag}-${triple.${system}}.tar.gz";
        hash = release.hashes.${system};
      };
    in
    lib.optionalAttrs (release.hashes ? ${system}) {
      packages.diskfoot-bin =
        pkgs.runCommand "diskfoot-bin-${release.version}"
          {
            meta = {
              description = "df, du and ncdu in one lazy disk-usage CLI/TUI (prebuilt)";
              mainProgram = "diskfoot";
              license = with lib.licenses; [
                mit
                asl20
              ];
              platforms = lib.attrNames release.hashes;
              sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
            };
          }
          ''
            tar -xzf ${tarball}
            install -Dm755 diskfoot $out/bin/diskfoot
          '';
    };
}
