import type { CSSProperties, ReactNode } from 'react';

interface FormFieldProps {
  label: string;
  htmlFor: string;
  children: ReactNode;
  style?: CSSProperties;
}

export function FormField({ label, htmlFor, children, style }: FormFieldProps) {
  return (
    <div className="form-group" style={style}>
      <label className="form-label" htmlFor={htmlFor}>{label}</label>
      {children}
    </div>
  );
}
