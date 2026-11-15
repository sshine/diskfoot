{
  description = "df, du and ncdu in one lazy disk-usage CLI/TUI";

  inputs = {
    nixpkgs.url = "https://nixos.org/channels/nixpkgs-unstable/nixexprs.tar.xz";

    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";

    import-tree.url = "github:vic/import-tree";

    # Pinned so the README generator does not drift with the nixpkgs channel.
    # A non-flake source: the tag's Cargo.lock makes the build reproducible.
    cargo-readme-src = {
      url = "github:webern/cargo-readme/v3.4.1";
      flake = false;
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crate2nix = {
      url = "github:nix-community/crate2nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.devshell.follows = "devshell";
    };

    hk-nix = {
      url = "github:nix-tools/hk-nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.treefmt-nix.follows = "treefmt-nix";
      inputs.import-tree.follows = "import-tree";
    };

    # The CI phase adds hashpinner here. Every input that takes nixpkgs follows
    # it; hk-nix also follows flake-parts, treefmt-nix and import-tree.
  };

  outputs =
    { flake-parts, import-tree, ... }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } (import-tree ./nix);
}
