import { Button } from "../ui/button";

export function CTABanner() {
  const rawBase = import.meta.env.BASE_URL;
  const base = rawBase.endsWith("/") ? rawBase : `${rawBase}/`;
  return (
    <section className="relative w-full px-6 py-28 sm:px-8 lg:py-36">
      {/* Background glow */}
      <div className="pointer-events-none absolute inset-0 overflow-hidden">
        <div className="absolute bottom-0 left-1/2 h-[500px] w-[800px] -translate-x-1/2 translate-y-1/2 rounded-full bg-accent/5 blur-[120px]" />
      </div>

      <div className="relative mx-auto flex max-w-2xl flex-col items-center gap-8 text-center">
        <p className="text-sm font-medium uppercase tracking-widest text-accent">
          Get started
        </p>
        <h2 className="text-3xl font-bold text-title sm:text-4xl">
          Ready to ship your first microVM?
        </h2>
        <p className="max-w-lg text-lg leading-relaxed text-body">
          From zero to a running Firecracker VM in minutes. mvm handles
          bootstrapping, Nix builds, and lifecycle management.
        </p>
        <div className="flex flex-wrap justify-center gap-4">
          <a href={`${base}getting-started/quickstart/`}>
            <Button size="lg">Quick Start Guide</Button>
          </a>
          <a href="https://github.com/auser/mvm" target="_blank" rel="noopener">
            <Button variant="outline" size="lg">
              View on GitHub
            </Button>
          </a>
        </div>
      </div>
    </section>
  );
}
