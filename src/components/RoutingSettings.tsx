import { useConnectionStore } from "@/state/connectionStore"

const INPUT =
  "h-8 w-full rounded-md bg-black/20 px-2 text-xs text-foreground ring-1 ring-white/10 outline-none focus:ring-primary disabled:opacity-50"
const AREA =
  "min-h-16 w-full resize-y rounded-md bg-black/20 px-2 py-1.5 text-xs text-foreground ring-1 ring-white/10 outline-none focus:ring-primary disabled:opacity-50"

/** Aether 1.5.0 DNS and routing controls. Each list accepts the exact
 * comma/newline-separated format documented by the core. */
export function RoutingSettings() {
  const profile = useConnectionStore((s) => s.profile)
  const status = useConnectionStore((s) => s.status)
  const setDns = useConnectionStore((s) => s.setDns)
  const setRouteBlock = useConnectionStore((s) => s.setRouteBlock)
  const setRouteDirect = useConnectionStore((s) => s.setRouteDirect)
  const setRoutesFile = useConnectionStore((s) => s.setRoutesFile)
  const locked = status.state !== "Idle" && status.state !== "Error"

  return (
    <div className="flex flex-col gap-2 rounded-md bg-black/10 p-2 ring-1 ring-white/10">
      <input
        type="text"
        value={profile.dns}
        disabled={locked}
        onChange={(e) => setDns(e.target.value)}
        placeholder="Tunnel DNS, e.g. 1.1.1.1,1.0.0.1 (optional)"
        className={INPUT}
        aria-label="Tunnel DNS resolvers"
      />
      <textarea
        value={profile.route_block}
        disabled={locked}
        onChange={(e) => setRouteBlock(e.target.value)}
        placeholder="Block: domains, CIDRs, ports… (optional)"
        className={AREA}
        aria-label="Blocked routes"
      />
      <textarea
        value={profile.route_direct}
        disabled={locked}
        onChange={(e) => setRouteDirect(e.target.value)}
        placeholder="Direct: banking, LAN, domestic sites… (optional)"
        className={AREA}
        aria-label="Direct routes"
      />
      <input
        type="text"
        value={profile.routes_file}
        disabled={locked}
        onChange={(e) => setRoutesFile(e.target.value)}
        placeholder="Rules file path (optional)"
        className={INPUT}
        aria-label="Routing rules file path"
      />
      <p className="text-[10px] leading-4 text-muted-foreground">
        Supports domain, IP/CIDR, <code>port:443</code>, <code>private</code>,
        and Aether&apos;s
        <code>full:</code>/<code>keyword:</code>/<code>regexp:</code> rules.
        Block wins over direct.
      </p>
    </div>
  )
}
