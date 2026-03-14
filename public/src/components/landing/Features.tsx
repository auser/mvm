import { Card, CardHeader, CardTitle, CardDescription } from "../ui/card";

const features = [
  {
    title: "Three-Layer Stack",
    description:
      "CLI on your host. On macOS or Linux without KVM, Lima provides /dev/kvm automatically. Native Linux skips Lima entirely. Firecracker runs your workloads.",
  },
  {
    title: "Nix-Based Builds",
    description:
      "Reproducible microVM images from Nix flakes. Cached builds — rebuilds are near-instant. Artifact sizes reported on every build.",
  },
  {
    title: "Service Builders",
    description:
      "mkPythonService, mkNodeService, and mkStaticSite — high-level helpers that return { package, service, healthCheck } for easy composition.",
  },
  {
    title: "Headless MicroVMs",
    description:
      "No SSH, ever. MicroVMs communicate via Firecracker vsock. The guest agent handles lifecycle with startup grace periods.",
  },
  {
    title: "Templates & Snapshots",
    description:
      "Build reusable images with size tracking. Snapshot for 1-2s boot. Inspect sizes and status with template info.",
  },
  {
    title: "Developer Experience",
    description:
      "Scaffold presets (Python, Node, HTTP, Postgres), actionable error hints, and system diagnostics via doctor.",
  },
];

export function Features() {
  return (
    <section className="w-full border-y border-edge/50 bg-raised px-6 py-28 sm:px-8 lg:py-36">
      <div className="mx-auto max-w-6xl">
        <h2 className="mb-16 text-center text-2xl font-semibold text-title sm:text-3xl lg:mb-20">
          How It Works
        </h2>
        <div className="grid gap-6 sm:grid-cols-2 lg:grid-cols-3 sm:gap-8">
          {features.map((f) => (
            <Card key={f.title}>
              <CardHeader>
                <CardTitle>{f.title}</CardTitle>
                <CardDescription>{f.description}</CardDescription>
              </CardHeader>
            </Card>
          ))}
        </div>
      </div>
    </section>
  );
}
