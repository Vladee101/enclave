/**
 * Minimal two-language i18n: no library, one context, typed dotted keys.
 * `t('backup.title')` autocompletes and type-checks against the dictionary
 * shape; `tPlural('backup.nFiles', n)` picks the grammatical form via
 * Intl.PluralRules. The choice persists in localStorage next to the other
 * `enclave.*` keys; the first run follows the system language.
 */
import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from 'react';
import { en, type Dict } from './en';
import { ru } from './ru';

export type Lang = 'en' | 'ru';
export type Vars = Record<string, string | number>;

const DICTS: Record<Lang, Dict> = { en, ru };
const STORAGE_KEY = 'enclave.lang';

// ─── Key typing ────────────────────────────────────────────────────────────────

/** Leaf paths of the dictionary: 'backup.title' (plural arrays are leaves). */
type Paths<T> = T extends readonly unknown[]
  ? never
  : T extends object
    ? {
        [K in keyof T & string]: T[K] extends string | readonly string[]
          ? K
          : `${K}.${Paths<T[K]>}`;
      }[keyof T & string]
    : never;

/** The value a path points to. */
type Value<T, P extends string> =
  P extends `${infer Head}.${infer Rest}`
    ? Head extends keyof T
      ? Value<T[Head], Rest>
      : never
    : P extends keyof T
      ? T[P]
      : never;

/** Paths of plain-string entries — the keys `t` accepts. */
export type TStringKey = Paths<Dict> extends infer P
  ? P extends string
    ? Value<Dict, P> extends string
      ? P
      : never
  : never
  : never;

/** Paths of plural entries — the keys `tPlural` accepts. */
export type TPluralKey = Paths<Dict> extends infer P
  ? P extends string
    ? Value<Dict, P> extends readonly string[]
      ? P
      : never
  : never
  : never;

/** Standalone `t` for helpers outside components (e.g. module-level mappers). */
export type TFunc = (key: TStringKey, vars?: Vars) => string;

// ─── Lookup ────────────────────────────────────────────────────────────────────

function lookup(lang: Lang, key: string): string | string[] {
  let cur: unknown = DICTS[lang];
  for (const part of key.split('.')) cur = (cur as Record<string, unknown>)[part];
  return cur as string | string[];
}

/** `{name}` substitution — split/join, since the tsconfig targets ES2020. */
function interpolate(template: string, vars: Vars): string {
  let out = template;
  for (const [name, value] of Object.entries(vars)) {
    out = out.split(`{${name}}`).join(String(value));
  }
  return out;
}

function initialLang(): Lang {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored === 'en' || stored === 'ru') return stored;
  } catch {
    /* storage unavailable — fall through to the system language */
  }
  return navigator.language.toLowerCase().startsWith('ru') ? 'ru' : 'en';
}

// ─── Context ───────────────────────────────────────────────────────────────────

interface I18nValue {
  lang: Lang;
  setLang: (lang: Lang) => void;
  t: TFunc;
  tPlural: (key: TPluralKey, count: number, vars?: Vars) => string;
  /** Locale-aware date/time for the current language. */
  formatDateTime: (iso: string) => string;
  /** Locale-aware number (grouping separators) for the current language. */
  formatNumber: (n: number) => string;
  /**
   * Text for an error from `invoke`: the core's `{code, params, message}`
   * (src-tauri/src/error.rs) in this language, else its message as is.
   */
  tError: (e: unknown) => string;
  /** Bytes as megabytes (1 decimal) or gigabytes (2): «194,5 МБ», "2.50 GB". */
  formatSize: (bytes: number, unit: 'MB' | 'GB') => string;
}

const I18nContext = createContext<I18nValue | null>(null);

export function I18nProvider({ children }: { children: ReactNode }) {
  const [lang, setLangState] = useState<Lang>(initialLang);

  useEffect(() => {
    document.documentElement.lang = lang;
  }, [lang]);

  const setLang = useCallback((next: Lang) => {
    setLangState(next);
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      /* ignore — the choice simply won't persist */
    }
  }, []);

  const t = useCallback(
    (key: TStringKey, vars?: Vars): string => {
      const template = lookup(lang, key) as string;
      return vars ? interpolate(template, vars) : template;
    },
    [lang],
  );

  const tPlural = useCallback(
    (key: TPluralKey, count: number, vars?: Vars): string => {
      const forms = lookup(lang, key) as string[];
      // English dictionaries carry 2 forms (one/other); Russian carries 3
      // (one/few/many) — length decides, so a new language just adds forms.
      const category = new Intl.PluralRules(lang === 'ru' ? 'ru-RU' : 'en-US').select(count);
      const index =
        forms.length <= 2
          ? category === 'one' ? 0 : 1
          : category === 'one' ? 0 : category === 'few' ? 1 : 2;
      return interpolate(forms[Math.min(index, forms.length - 1)], { count, ...vars });
    },
    [lang],
  );

  const formatDateTime = useCallback(
    (iso: string) => new Date(iso).toLocaleString(lang === 'ru' ? 'ru-RU' : 'en-US'),
    [lang],
  );

  const formatNumber = useCallback(
    (n: number) => n.toLocaleString(lang === 'ru' ? 'ru-RU' : 'en-US'),
    [lang],
  );

  const formatSize = useCallback(
    (bytes: number, unit: 'MB' | 'GB') => {
      const digits = unit === 'MB' ? 1 : 2;
      const n = (bytes / (unit === 'MB' ? 1e6 : 1e9)).toLocaleString(lang === 'ru' ? 'ru-RU' : 'en-US', {
        minimumFractionDigits: digits,
        maximumFractionDigits: digits,
      });
      return interpolate(lookup(lang, unit === 'MB' ? 'common.unitMB' : 'common.unitGB') as string, { n });
    },
    [lang],
  );

  const tError = useCallback(
    (e: unknown): string => {
      if (e && typeof e === 'object' && 'code' in e) {
        const { code, params, message } = e as { code: string; params?: Vars; message?: string };
        const template = (DICTS[lang].errors as Record<string, string>)[code];
        if (template) return interpolate(template, params ?? {});
        if (message) return message;
      }
      if (e instanceof Error) return e.message;
      return String(e);
    },
    [lang],
  );

  return (
    <I18nContext.Provider value={{ lang, setLang, t, tPlural, tError, formatDateTime, formatNumber, formatSize }}>
      {children}
    </I18nContext.Provider>
  );
}

export function useI18n(): I18nValue {
  const ctx = useContext(I18nContext);
  if (!ctx) throw new Error('useI18n must be used within I18nProvider');
  return ctx;
}

// ─── Language switch ───────────────────────────────────────────────────────────

/** RU | EN pill; placed in the sidebar, on the login card and in ModelSetup. */
export function LangToggle({ className }: { className?: string }) {
  const { lang, setLang } = useI18n();
  return (
    <div className={`lang-toggle${className ? ` ${className}` : ''}`} role="group" aria-label="Language / Язык">
      <button type="button" className={lang === 'ru' ? 'on' : ''} aria-pressed={lang === 'ru'} onClick={() => setLang('ru')}>
        RU
      </button>
      <button type="button" className={lang === 'en' ? 'on' : ''} aria-pressed={lang === 'en'} onClick={() => setLang('en')}>
        EN
      </button>
    </div>
  );
}
