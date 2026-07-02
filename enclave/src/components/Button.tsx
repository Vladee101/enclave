import type { ButtonHTMLAttributes } from 'react';
import { Spinner } from './Spinner';

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: 'primary' | 'ghost';
  loading?: boolean;
  fullWidth?: boolean;
  /** Spinner size while loading; defaults to the CSS default (20px). */
  spinnerSize?: number;
}

export function Button({
  variant = 'primary',
  loading = false,
  fullWidth = false,
  spinnerSize,
  className,
  disabled,
  children,
  ...rest
}: ButtonProps) {
  const cls = ['btn', `btn-${variant}`, fullWidth && 'w-full', className]
    .filter(Boolean)
    .join(' ');

  return (
    <button className={cls} disabled={disabled || loading} {...rest}>
      {loading ? <Spinner size={spinnerSize} /> : children}
    </button>
  );
}
