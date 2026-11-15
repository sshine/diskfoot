# Every binary the workspace ships, built through crate2nix.
#
# IFD form: the dependency graph is generated at evaluation. This is a small
# single binary, so there is no committed Cargo.nix to drift and no overlay: an
# overlay cannot call appliedCargoNix without this flake's system and inputs.
{ inputs, ... }:
{
  perSystem =
    { system, lib, ... }:
    let
      cargoNix = inputs.crate2nix.tools.${system}.appliedCargoNix {
        name = "diskfoot";
        src = ../.;
      };

      binaries = [ "diskfoot" ];

      built = lib.genAttrs binaries (crate: cargoNix.workspaceMembers.${crate}.build);
    in
    {
      packages = built // {
        default = built.diskfoot;
      };

      # One check per binary, so `nix flake check` builds every shipped crate.
      checks = built;

      apps.default = {
        type = "app";
        # crate2nix does not set meta.mainProgram, so name the binary.
        program = "${built.diskfoot}/bin/diskfoot";
      };
    };
}
