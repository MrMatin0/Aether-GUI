import { useMemo, useState, type FormEvent } from "react"
import { invoke } from "@tauri-apps/api/core"
import { Button } from "@/components/ui/button"
import { useConnectionStore } from "@/state/connectionStore"

/** Bridges Aether 1.5.0's terminal-only Zero Trust email-code prompt into the
 * GUI. A new prompt signal is emitted after each rejected code, allowing the
 * user to retry without relaunching the tunnel. */
export function AccessCodePrompt() {
  const logs = useConnectionStore((s) => s.logs)
  const [code, setCode] = useState("")
  const [submittedFor, setSubmittedFor] = useState(0)
  const [sending, setSending] = useState(false)
  const promptCount = useMemo(
    () =>
      logs.filter((log) => log.line === "[gui] Zero Trust access code required")
        .length,
    [logs]
  )
  const waiting = promptCount > submittedFor

  if (!waiting) return null

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!code.trim() || sending) return
    setSending(true)
    try {
      await invoke("submit_access_code", { code })
      setCode("")
      setSubmittedFor(promptCount)
    } finally {
      setSending(false)
    }
  }

  return (
    <form onSubmit={submit} className="flex w-full max-w-xs items-center gap-2">
      <input
        autoFocus
        type="text"
        inputMode="numeric"
        autoComplete="one-time-code"
        value={code}
        onChange={(e) => setCode(e.target.value)}
        placeholder="Enter the code sent to your email"
        className="h-8 min-w-0 flex-1 rounded-md bg-black/30 px-2 text-center text-xs text-foreground ring-1 ring-primary/50 outline-none focus:ring-primary"
        aria-label="Zero Trust email code"
      />
      <Button type="submit" size="sm" disabled={!code.trim() || sending}>
        {sending ? "Sending…" : "Verify"}
      </Button>
    </form>
  )
}
