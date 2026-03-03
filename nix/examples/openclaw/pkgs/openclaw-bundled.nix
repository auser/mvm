# Wraps the nix-openclaw gateway package with an esbuild bundle for
# fast startup on Firecracker's virtio-block (loading 800+ code-split
# chunks individually is too slow on nested virtualization).
#
# The upstream nix-openclaw flake handles source fetching, pnpm deps,
# and the full build.  This derivation just adds a single bundling step.

{ lib
, stdenv
, nodejs_22
, esbuild
, makeWrapper
, openclaw-gateway
}:

stdenv.mkDerivation {
  pname = "openclaw-bundled";
  version = openclaw-gateway.version;

  src = openclaw-gateway;
  dontUnpack = true;

  nativeBuildInputs = [ nodejs_22 esbuild makeWrapper ];

  installPhase = ''
    runHook preInstall

    mkdir -p $out/lib/openclaw $out/bin

    # Copy the upstream package contents.
    cp -r $src/lib/openclaw/. $out/lib/openclaw/
    chmod -R u+w $out/lib/openclaw

    # Bundle dist/ into a single ESM file for fast virtio-block loading.
    ENTRY="$out/lib/openclaw/dist/index.js"
    BUNDLE="$out/lib/openclaw/dist/openclaw-bundle.mjs"

    # Bundle only OpenClaw's own code-split chunks into a single file.
    # --packages=external keeps all node_modules imports as-is, avoiding
    # pnpm resolution issues while still consolidating the 800+ chunks
    # into one file for fast virtio-block loading.
    echo "Bundling $ENTRY with esbuild..."
    esbuild "$ENTRY" \
      --bundle \
      --packages=external \
      --platform=node \
      --target=node22 \
      --format=esm \
      --outfile="$BUNDLE" \
      --log-level=warning 2>&1 || {
        echo "WARNING: esbuild bundle failed, falling back to unbundled entry"
        BUNDLE="$ENTRY"
      }

    echo "Bundle: $(ls -lh "$BUNDLE" | awk '{print $5}')"

    makeWrapper ${nodejs_22}/bin/node $out/bin/openclaw \
      --add-flags "$BUNDLE" \
      --set NODE_PATH "$out/lib/openclaw/node_modules" \
      --set-default OPENCLAW_NIX_MODE "1" \
      --run "cd '$out/lib/openclaw'"

    runHook postInstall
  '';

  meta = openclaw-gateway.meta // {
    description = "OpenClaw gateway (esbuild-bundled for microVM)";
  };
}
