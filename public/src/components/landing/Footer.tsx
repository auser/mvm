import {
  BookOpen,
  ExternalLink,
  Github,
  Newspaper,
  ShieldCheck,
} from "lucide-react";

const linkGroups = [
  {
    title: "Resources",
    links: [
      { label: "This Week in MicroVMs", href: "https://this-week-in-microvms.com" },
      { label: "Nix for mvm", href: "/getting-started/nix-for-mvm/" },
      { label: "Architecture", href: "/architecture/overview/" },
      { label: "GitHub", href: "https://github.com/tinylabscom/mvm" },
    ],
  },
  {
    title: "Explore",
    links: [
      { label: "Architecture", href: "/architecture/overview/" },
      { label: "Nix and OCI", href: "/guides/nix-and-oci/" },
      { label: "Agent Sandbox", href: "/tutorials/agent-sandbox/" },
      { label: "Audit and Receipts", href: "/guides/audit-and-receipts/" },
    ],
  },
  {
    title: "Community",
    links: [
      { label: "This Week in MicroVMs", href: "https://this-week-in-microvms.com" },
      { label: "GitHub", href: "https://github.com/tinylabscom/mvm" },
      { label: "Issues", href: "https://github.com/tinylabscom/mvm/issues" },
      { label: "Releases", href: "https://github.com/tinylabscom/mvm/releases" },
    ],
  },
];

const socialLinks = [
  { label: "GitHub", href: "https://github.com/tinylabscom/mvm", icon: Github },
  { label: "Blog", href: "/blog/", icon: Newspaper },
  { label: "Docs", href: "/getting-started/installation/", icon: BookOpen },
  { label: "MicroVMs weekly", href: "https://this-week-in-microvms.com", icon: ShieldCheck },
];

function withBase(path: string, base: string) {
  if (path.startsWith("http")) return path;
  return `${base}${path.replace(/^\//, "")}`;
}

export function Footer() {
  const rawBase = import.meta.env.BASE_URL;
  const base = rawBase.endsWith("/") ? rawBase : `${rawBase}/`;

  return (
    <footer className="landing-footer border-t border-edge bg-canvas px-6 py-16 sm:px-8 lg:py-20">
      <div className="mx-auto max-w-5xl">
        <div className="grid w-full gap-12 sm:grid-cols-2 lg:grid-cols-4 lg:gap-10">
          <div className="max-w-sm">
            <a href={base} className="font-mono text-3xl font-bold text-title no-underline">
              mvm
            </a>
            <p className="mt-5 max-w-sm text-base leading-7 text-body">
              Secure, reproducible microVMs for running untrusted code without
              turning every developer into an infrastructure operator.
            </p>

            <div className="mt-6 flex flex-wrap gap-3">
              {socialLinks.map(({ label, href, icon: Icon }) => {
                const target = withBase(href, base);
                const external = href.startsWith("http");
                return (
                  <a
                    key={label}
                    href={target}
                    target={external ? "_blank" : undefined}
                    rel={external ? "noopener" : undefined}
                    className="inline-flex h-9 w-9 items-center justify-center rounded-md text-label transition hover:bg-canvas hover:text-accent"
                    aria-label={label}
                  >
                    <Icon size={18} strokeWidth={2} />
                  </a>
                );
              })}
            </div>
          </div>

          {linkGroups.map((group) => (
            <nav key={group.title} aria-label={group.title}>
              <h2 className="text-sm font-semibold text-title">{group.title}</h2>
              <ul className="mt-5 space-y-3.5">
                {group.links.map((link) => {
                  const external = link.href.startsWith("http");
                  return (
                    <li key={link.label}>
                      <a
                        href={withBase(link.href, base)}
                        target={external ? "_blank" : undefined}
                        rel={external ? "noopener" : undefined}
                        className="text-sm leading-6 text-body no-underline transition hover:text-accent"
                      >
                        <span>{link.label}</span>
                        {external && (
                          <ExternalLink
                            size={13}
                            strokeWidth={2}
                            className="ml-1 inline-block align-[-2px]"
                          />
                        )}
                      </a>
                    </li>
                  );
                })}
              </ul>
            </nav>
          ))}
        </div>

        <div className="mt-14 flex flex-col gap-4 border-t border-edge pt-6 text-sm text-label lg:flex-row lg:items-center lg:justify-between">
          <p>Built by Tiny Labs.</p>
          <nav className="flex flex-wrap gap-x-8 gap-y-3" aria-label="Legal">
            <a href={`${base}security/threat-model/`} className="text-label no-underline transition hover:text-accent">
              Threat model
            </a>
            <a href={`${base}security/claim-ledger/`} className="text-label no-underline transition hover:text-accent">
              Security claims
            </a>
            <a href={`${base}resources/changelog/`} className="text-label no-underline transition hover:text-accent">
              Changelog
            </a>
          </nav>
        </div>
      </div>
    </footer>
  );
}
