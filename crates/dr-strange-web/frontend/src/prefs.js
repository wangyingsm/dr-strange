// Tiny localStorage-backed preference helpers so dropdown choices (providers,
// modes, the current plane, …) persist across reloads. Keys are namespaced;
// everything is stored as a string. Fails silent when storage is unavailable
// (private mode, disabled), falling back to the provided default.

export function loadPref(key, fallback) {
  try {
    const v = localStorage.getItem(`drsg:${key}`)
    return v === null ? fallback : v
  } catch {
    return fallback
  }
}

export function savePref(key, value) {
  try {
    localStorage.setItem(`drsg:${key}`, String(value))
  } catch {
    // ignore
  }
}
