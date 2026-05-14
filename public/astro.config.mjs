import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import tailwindcss from "@tailwindcss/vite";
import react from "@astrojs/react";

export default defineConfig({
  site: "https://gomicrovm.com",
  base: "/",
  vite: {
    plugins: [tailwindcss()],
  },
  integrations: [
    starlight({
      title: "mvm",
      logo: {
        light: "./src/assets/logo-light.svg",
        dark: "./src/assets/logo-dark.svg",
        replacesTitle: true,
      },
      social: [
        { icon: "github", label: "GitHub", href: "https://github.com/tinylabscom/mvm" },
      ],
      expressiveCode: {
        themes: ["github-dark"],
        styleOverrides: {
          borderColor: "#30363d", // overridden by custom.css var(--color-border)
          borderRadius: "0.75rem",
        },
      },
      customCss: ["./tailwind.css", "./src/styles/custom.css"],
      components: {
        Hero: "./src/overrides/Hero.astro",
        Header: "./src/overrides/Header.astro",
      },
      // No force-theme script. Starlight's theme picker writes
      // data-theme="auto"|"light"|"dark" on <html>; tailwind.css
      // handles each via the token system documented there. The
      // previous iteration force-locked dark via this slot; the
      // new token system supports both modes natively.
      sidebar: [
        {
          label: "Getting Started",
          items: [
            { label: "Installation", slug: "getting-started/installation" },
            { label: "Quick Start", slug: "getting-started/quickstart" },
            { label: "Your First MicroVM", slug: "getting-started/first-microvm" },
            { label: "Connect an LLM", slug: "getting-started/connect-an-llm" },
            { label: "Nix for mvm", slug: "getting-started/nix-for-mvm" },
          ],
        },
        {
          label: "Install",
          items: [
            { label: "Linux", slug: "install/linux" },
            { label: "macOS", slug: "install/macos" },
            { label: "Windows (WSL2)", slug: "install/windows" },
          ],
        },
        {
          label: "Working in the MicroVM",
          items: [
            { label: "Overview", slug: "working" },
            { label: "Run commands & processes", slug: "working/commands" },
            { label: "Filesystem operations", slug: "working/filesystem" },
            { label: "Network & exposing ports", slug: "working/network" },
            { label: "Persistence, pause & resume", slug: "working/persistence" },
            { label: "Snapshots", slug: "working/snapshots" },
          ],
        },
        {
          label: "Console",
          items: [
            { label: "Overview", slug: "console" },
            { label: "Attach to a microVM", slug: "console/attach" },
            { label: "Transparent rebuilds", slug: "console/transparent-rebuild" },
          ],
        },
        {
          label: "Templates",
          items: [
            { label: "Overview", slug: "templates" },
            { label: "Create a template", slug: "templates/create" },
            { label: "Build & list", slug: "templates/build" },
            { label: "Lifecycle", slug: "templates/lifecycle" },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "Writing Nix Flakes", slug: "guides/nix-flakes" },
            { label: "From Workload IR to MicroVM Image", slug: "guides/ir-to-image" },
            { label: "Building MicroVM Images", slug: "guides/building-microvm-images" },
            { label: "Sandboxed Exec", slug: "guides/exec" },
            { label: "Config & Secrets", slug: "guides/config-secrets" },
            { label: "Manifests", slug: "guides/manifests" },
            { label: "Networking", slug: "guides/networking" },
            { label: "Dev Image", slug: "guides/dev-image" },
            { label: "Verify Release", slug: "guides/verify-release" },
            { label: "Airgapped Bootstrap", slug: "guides/airgapped-bootstrap" },
            { label: "Troubleshooting", slug: "guides/troubleshooting" },
            { label: "Windows: WSL2 walkthrough", slug: "guides/windows-wsl2" },
            { label: "Windows: troubleshooting", slug: "guides/windows-troubleshooting" },
          ],
        },
        {
          label: "Examples",
          items: [
            { label: "Overview", slug: "examples" },
            { label: "Sandbox for an AI agent", slug: "examples/ai-agent-sandbox" },
            { label: "CI/CD ephemeral builder", slug: "examples/ci-cd-ephemeral-builder" },
            { label: "Reproducible dev VM from a flake", slug: "examples/dev-vm-from-flake" },
            { label: "Code interpreter pattern", slug: "examples/code-interpreter" },
          ],
        },
        {
          label: "Security",
          items: [
            { label: "Matryoshka Model", slug: "security/matryoshka" },
            { label: "Threat model", slug: "security/threat-model" },
            { label: "Seven CI claims", slug: "security/ci-claims" },
            { label: "Verified boot", slug: "security/verified-boot" },
            { label: "Sandbox parity status", slug: "security/sandbox-parity-status" },
          ],
        },
        {
          label: "Deploy",
          items: [
            { label: "AWS EC2", slug: "deploy/aws" },
            { label: "Google Cloud Platform", slug: "deploy/gcp" },
            { label: "Hetzner Cloud", slug: "deploy/hetzner" },
            { label: "Ubicloud", slug: "deploy/ubicloud" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "CLI Commands", slug: "reference/cli-commands" },
            { label: "Programmatic Use", slug: "reference/programmatic-use" },
            { label: "Architecture", slug: "reference/architecture" },
            { label: "Filesystem & Drives", slug: "reference/filesystem" },
            { label: "Guest Agent", slug: "reference/guest-agent" },
            { label: "Limits & Resources", slug: "reference/limits" },
          ],
        },
        {
          label: "Contributing",
          items: [
            { label: "Development Guide", slug: "contributing/development" },
            { label: "ADR-001: Multi-Backend VMs", slug: "contributing/adr/001-multi-backend" },
            { label: "ADR-013: libkrun + libkrun + microvm.nix", slug: "contributing/adr/013-libkrun-pivot" },
          ],
        },
      ],
    }),
    react(),
  ],
});
