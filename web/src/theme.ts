/** Light/dark theme: one attribute on <html>, remembered in localStorage, following the OS by default. */
export type Theme = "light" | "dark";

const STORAGE_KEY = "stackbox-theme";
const DARK_QUERY = "(prefers-color-scheme: dark)";

/** The operator's own choice, or null when the theme still follows the OS. */
export function storedTheme(): Theme | null {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    // Private mode or a blocked storage: fall back to the OS preference.
    return null;
  }
}

export function preferredTheme(): Theme {
  const stored = storedTheme();
  if (stored) return stored;
  const dark =
    typeof window.matchMedia === "function" &&
    window.matchMedia(DARK_QUERY).matches;
  return dark ? "dark" : "light";
}

/** Applies a theme without remembering it, so the OS preference keeps winning on reload. */
export function applyTheme(theme: Theme) {
  const root = document.documentElement;
  root.dataset.theme = theme;
  root.style.colorScheme = theme;
  document
    .querySelector('meta[name="theme-color"]')
    ?.setAttribute("content", theme === "dark" ? "#0f1210" : "#f6f6f3");
}

/** Reports OS preference changes for as long as the operator has not chosen a theme.
 * Returns the unsubscribe for the caller's effect. */
export function watchSystemTheme(onChange: (theme: Theme) => void) {
  if (typeof window.matchMedia !== "function") return () => {};
  const query = window.matchMedia(DARK_QUERY);
  const listener = (event: MediaQueryListEvent) => {
    // A stored choice outranks the OS, so it stops the follow.
    if (storedTheme()) return;
    onChange(event.matches ? "dark" : "light");
  };
  query.addEventListener("change", listener);
  return () => query.removeEventListener("change", listener);
}

/** Applies the theme and remembers it as the operator's choice. */
export function setTheme(theme: Theme) {
  applyTheme(theme);
  try {
    localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // Storage is unavailable; the theme still applies to this page.
  }
}
