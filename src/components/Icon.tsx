import type { ReactNode } from "react";

const shapes = {
  swap: <path d="M3 7h18m-4-4 4 4-4 4M21 17H3m4-4-4 4 4 4" />,
  add: <path d="M12 4v16M4 12h16" />,
  save: <><path d="M4 3h13l4 4v14H3V3ZM7 3v6h10V3M7 21v-7h10v7" /></>,
  check: <path d="m5 12 4 4L19 6" />,
  up: <path d="m5 15 7-7 7 7" />,
  down: <path d="m5 9 7 7 7-7" />,
  home: <><path d="m3 10 9-7 9 7v10a1 1 0 0 1-1 1h-5v-7H9v7H4a1 1 0 0 1-1-1Z" /></>,
  history: <><path d="M3 11a9 9 0 1 1 2.5 7M3 4v7h7M12 7v5l3 2" /></>,
  dictionary: <><path d="M12 5C8 2 4 3 2 4v16c3-1.5 6-1 10 1 4-2 7-2.5 10-1V4c-2-1-6-2-10 1Zm0 0v16" /></>,
  settings: <><path d="m9 3-1 3-3 1-2 4 2 2v4l4 2 3-1 3 1 4-2v-4l2-2-2-4-3-1-1-3Z" /><circle cx="12" cy="11" r="3" /></>,
  microphone: <><rect x="9" y="2" width="6" height="12" rx="3" /><path d="M5 10v2a7 7 0 0 0 14 0v-2M12 19v3M8 22h8" /></>,
  keyboard: <><rect x="2" y="5" width="20" height="14" rx="2" /><path d="M6 9h.01M10 9h.01M14 9h.01M18 9h.01M6 12h.01M10 12h.01M14 12h.01M18 12h.01M7 16h10" /></>,
  writing: <><path d="m15 4 5 5M4 20l5-1L21 7a2 2 0 0 0-5-5L4 14ZM4 20h16" /></>,
  model: <><rect x="6" y="6" width="12" height="12" rx="2" /><path d="M9 1v5M15 1v5M9 18v5M15 18v5M1 9h5M1 15h5M18 9h5M18 15h5M10 10h4v4h-4Z" /></>,
  shield: <><path d="m12 2 9 4v6c0 5-9 10-9 10S3 17 3 12V6ZM8 12l3 3 5-6" /></>,
  appearance: <><circle cx="12" cy="12" r="9" /><path d="M12 3v18" /><path d="M12 3a9 9 0 0 1 0 18Z" fill="currentColor" stroke="none" /></>,
  setup: <><path d="M9 4H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2h-4" /><rect x="9" y="2" width="6" height="4" rx="1" /><path d="m8 13 3 3 5-6" /></>,
  diagnostics: <><path d="M3 3v18h18M5 14h3l3-7 4 11 3-7h3" /></>,
  copy: <><rect x="8" y="8" width="13" height="13" rx="2" /><path d="M16 8V3H3v13h5" /></>,
  raw: <><path d="M8 3H4v18h16V8l-5-5H8ZM14 3v6h6M7 13h10M7 17h7" /></>,
  retry: <><path d="M3 10a9 9 0 1 1 2 8M3 3v7h7" /></>,
  play: <path d="m8 4 13 8-13 8Z" />,
  download: <><path d="M12 3v12m-5-5 5 5 5-5M4 16v5h16v-5" /></>,
  trash: <><path d="M3 6h18M9 6V3h6v3M6 6l1 15h10l1-15M10 10v7M14 10v7" /></>,
  close: <path d="m6 6 12 12M6 18 18 6" />,
  search: <><circle cx="10.5" cy="10.5" r="7.5" /><path d="m16 16 6 6" /></>,
  filter: <><path d="M3 4h18l-7 8v8l-4-2v-6Z" /></>,
  refresh: <><path d="M3 10a9 9 0 0 1 15-6l3 3M21 2v5h-5M21 14a9 9 0 0 1-15 6l-3-3M3 22v-5h5" /></>,
  folder: <path d="M3 6h6l2 2h10v12H3Zm0 0V3h7l2 3h9v2" />,
  database: <><ellipse cx="12" cy="5" rx="9" ry="3" /><path d="M3 5v14c0 4 18 4 18 0V5M3 12c0 4 18 4 18 0" /></>,
  all: <><rect x="3" y="3" width="7" height="7" rx="1" /><rect x="14" y="3" width="7" height="7" rx="1" /><rect x="3" y="14" width="7" height="7" rx="1" /><rect x="14" y="14" width="7" height="7" rx="1" /></>,
  translate: <><path d="M2 5h12M8 2v3M4 5c0 5 6 9 9 10M12 5c0 5-6 9-10 10M14 22l4-11 4 11M16 18h4" /></>,
  ask: <><path d="M21 12a9 9 0 0 1-9 9H3v-9a9 9 0 1 1 18 0Z" /><path d="M9 9a3 3 0 0 1 6 0c0 2-3 2-3 4M12 17h.01" /></>,
} satisfies Record<string, ReactNode>;

export type IconName = keyof typeof shapes;
export function Icon({ name, size = 20 }: { name: IconName; size?: number }) {
  return <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">{shapes[name]}</svg>;
}
