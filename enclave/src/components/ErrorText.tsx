interface ErrorTextProps {
  children: string;
}

export function ErrorText({ children }: ErrorTextProps) {
  return (
    <div style={{ color: 'var(--error)', fontSize: 13, marginBottom: 12 }}>
      {children}
    </div>
  );
}
