import { afterEach, expect, test, vi } from "vitest";
import {
  applyTheme,
  preferredTheme,
  setTheme,
  storedTheme,
  watchSystemTheme,
  type Theme,
} from "./theme";

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.theme;
  document.documentElement.removeAttribute("style");
  vi.unstubAllGlobals();
});

/** A matchMedia stand-in whose OS preference the test can flip. */
function systemTheme(initialDark: boolean) {
  const listeners = new Set<(event: { matches: boolean }) => void>();
  const state = { dark: initialDark };
  vi.stubGlobal("matchMedia", (query: string) => ({
    media: query,
    get matches() {
      return state.dark;
    },
    addEventListener: (
      _: string,
      listener: (event: { matches: boolean }) => void,
    ) => listeners.add(listener),
    removeEventListener: (
      _: string,
      listener: (event: { matches: boolean }) => void,
    ) => listeners.delete(listener),
  }));
  return {
    listeners,
    flip(dark: boolean) {
      state.dark = dark;
      for (const listener of listeners) listener({ matches: dark });
    },
  };
}

test("remembers the operator's choice and prefers it over the OS", () => {
  systemTheme(false);
  expect(storedTheme()).toBeNull();
  expect(preferredTheme()).toBe("light");

  setTheme("dark");
  expect(storedTheme()).toBe("dark");
  expect(document.documentElement.dataset.theme).toBe("dark");
  // The remembered choice outranks the OS preference, which is still light.
  expect(preferredTheme()).toBe("dark");

  // Only the two known values count as a choice.
  localStorage.setItem("stackbox-theme", "sepia");
  expect(storedTheme()).toBeNull();
  expect(preferredTheme()).toBe("light");
});

test("falls back to the OS preference and follows it until a theme is chosen", () => {
  const system = systemTheme(false);
  const seen: Theme[] = [];
  const stop = watchSystemTheme((theme) => seen.push(theme));

  expect(preferredTheme()).toBe("light");
  system.flip(true);
  expect(seen).toEqual(["dark"]);

  // A stored choice stops the follow, so a later OS change is ignored.
  setTheme("light");
  system.flip(false);
  expect(seen).toEqual(["dark"]);

  stop();
  expect(system.listeners.size).toBe(0);
});

test("applies a theme without remembering it", () => {
  systemTheme(false);
  applyTheme("dark");
  expect(document.documentElement.dataset.theme).toBe("dark");
  expect(document.documentElement.style.colorScheme).toBe("dark");
  // Following the OS on reload still wins, because nothing was stored.
  expect(storedTheme()).toBeNull();
});
