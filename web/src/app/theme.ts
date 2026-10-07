import { useEffect } from 'react';
import { useUi } from '../store/ui';
import { useMediaQuery } from './ui/primitives';

export function useResolvedTheme(): 'light' | 'dark' {
  const pref = useUi((s) => s.settings.theme);
  const systemDark = useMediaQuery('(prefers-color-scheme: dark)');
  return pref === 'system' ? (systemDark ? 'dark' : 'light') : pref;
}

/** Applies the resolved theme to <html> and the theme-color meta tags. */
export function useApplyTheme(): void {
  const theme = useResolvedTheme();
  useEffect(() => {
    document.documentElement.classList.toggle('dark', theme === 'dark');
    const color = theme === 'dark' ? '#0f0f10' : '#ffffff';
    document.querySelectorAll('meta[name="theme-color"]').forEach((m) => m.setAttribute('content', color));
  }, [theme]);
}
