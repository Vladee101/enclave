import type { CSSProperties } from 'react';

interface SpinnerProps {
  size?: number;
  borderWidth?: number;
}

export function Spinner({ size, borderWidth }: SpinnerProps) {
  const style: CSSProperties = {};
  if (size !== undefined) {
    style.width = size;
    style.height = size;
  }
  if (borderWidth !== undefined) style.borderWidth = borderWidth;
  return <span className="spinner" style={style} />;
}
