{
  description = "Atlas media service development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustc
              cargo
              rustfmt
              clippy
              rust-analyzer
              imagemagick
              sqlite
              pkg-config

              # The example consumers are Python, and they were silently
              # borrowing whatever `python3` happened to be on PATH, which is
              # not reproducible and is not this flake's version. Pin them here
              # so `nix develop` is the whole story.
              python3
              python3Packages.flask
              python3Packages.python-dotenv
              python3Packages.requests
              python3Packages.pytest
              python3Packages.ruff
              python3Packages.ty
            ];

            # The pinned ImageMagick has to carry the codecs Atlas needs. A
            # nixpkgs bump that dropped libheif would otherwise show up as a
            # server that refuses to start, or as a phone photograph that
            # cannot be uploaded, long after the cause. Say it here, where
            # the reason can still be read.
            #
            # The check is the same one Atlas makes at startup
            # (`ImageProcessor::verify_formats`); it lives in a script so
            # that the dev shell and CI make it the same way.
            shellHook = ''
              ${builtins.readFile ./scripts/check-magick-codecs.sh}
            '';
          };
        });
    };
}
