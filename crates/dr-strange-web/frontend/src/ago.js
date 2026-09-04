// How long ago something happened, in the words a reader would use.
//
// Coarse on purpose: a history list is scanned, not read. The exact instant
// is in the `title`, and past a month the date itself is more use than a
// count of days.

const MINUTE = 60_000
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR

/**
 * `at` (epoch milliseconds) as a phrase relative to `now`.
 *
 * A time in the future reads as "just now" rather than counting backwards:
 * clocks disagree by a few seconds and a list is not the place to say so.
 */
export function ago(at, now = Date.now()) {
  const d = now - at
  if (d < 45 * 1000) return 'just now'
  if (d < 90 * MINUTE) return `${Math.round(d / MINUTE)}m ago`
  if (d < 36 * HOUR) return `${Math.round(d / HOUR)}h ago`
  if (d < 30 * DAY) return `${Math.round(d / DAY)}d ago`
  return new Date(at).toISOString().slice(0, 10)
}
