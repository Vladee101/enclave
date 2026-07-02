import type { ReactNode } from 'react';

interface BadgeProps {
  /** e.g. 'badge-success', 'badge-warning' — see styles/index.css */
  cls: string;
  children: ReactNode;
}

export function Badge({ cls, children }: BadgeProps) {
  return <span className={`badge ${cls}`}>{children}</span>;
}
