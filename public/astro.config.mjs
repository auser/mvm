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
        { icon: "github", label: "GitHub", href: "https://github.com/auser/mvm" },
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
            { label: "Nix for mvm", slug: "getting-started/nix-for-mvm" },
            { label: "Your First MicroVM", slug: "getting-started/first-microvm" },
          ],
        },
        {
          label: "Install",
          items: [
            { label: "Windows (WSL2)", slug: "install/windows" },
          ],
        },
        {
          // `guides/templates` removed — its page wasn't ported over
          // from the previous iteration. Re-add when Phase 5 (DX
          // layer) ships the templates guide content.
          label: "Guides",
          items: [
            { label: "Writing Nix Flakes", slug: "guides/nix-flakes" },
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
          label: "Security",
          items: [
            { label: "Matryoshka Model", slug: "security/matryoshka" },
          ],
        },
        {
          label: "Deploy",
          items: [
            { label: "AWS EC2", slug: "deploy/aws" },
            { label: "Ubicloud", slug: "deploy/ubicloud" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "CLI Commands", slug: "reference/cli-commands" },
            { label: "Architecture", slug: "reference/architecture" },
            { label: "Filesystem & Drives", slug: "reference/filesystem" },
            { label: "Guest Agent", slug: "reference/guest-agent" },
          ],
        },
        {
          label: "Contributing",
          items: [
            { label: "Development Guide", slug: "contributing/development" },
            { label: "ADR-001: Multi-Backend VMs", slug: "contributing/adr/001-multi-backend" },
            { label: "ADR-013: microsandbox + libkrun + microvm.nix", slug: "contributing/adr/013-microsandbox-pivot" },
          ],
        },
      ],
    }),
    react(),
  ],
});
