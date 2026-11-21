import { gameHue, monogram } from "../lib/format";
import type { ButtonHTMLAttributes, ReactNode } from "react";

export const cx = (...c: (string | false | null | undefined)[]) => c.filter(Boolean).join(" ");

type Variant = "primary" | "secondary" | "ghost" | "danger";

const variants: Record<Variant, string> = {
  primary: "bg-accent text-accent-ink hover:bg-accent-dim font-semibold",
  secondary: "bg-raised text-fg border border-line hover:bg-hover hover:border-line-strong",
  ghost: "text-muted hover:text-fg hover:bg-hover",
  danger: "bg-rec/12 text-rec border border-rec/30 hover:bg-rec/20",
};

export function Button({
  variant = "secondary",
  size = "md",
  className,
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: "sm" | "md" | "lg" }) {
  return (
    <button
      {...rest}
      className={cx(
        "inline-flex items-center gap-2 whitespace-nowrap rounded-ctl transition-[background-color,border-color,color,transform] duration-150 active:translate-y-px disabled:pointer-events-none disabled:opacity-40",
        size === "sm" && "h-7 px-2.5 text-[13px]",
        size === "md" && "h-9 px-3.5 text-[13.5px]",
        size === "lg" && "h-11 px-5 text-[14.5px]",
        variants[variant],
        !className?.includes("justify-") && "justify-center",
        className,
      )}
    >
      {children}
    </button>
  );
}

export function IconButton({
  label,
  className,
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { label: string }) {
  return (
    <button
      {...rest}
      aria-label={label}
      title={label}
      className={cx(
        "inline-flex size-8 items-center justify-center rounded-ctl text-muted transition-colors hover:bg-hover hover:text-fg disabled:opacity-40",
        className,
      )}
    >
      {children}
    </button>
  );
}

export function KeyCap({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <kbd
      className={cx(
        "inline-flex h-[22px] min-w-[22px] items-center justify-center rounded-[4px] border border-line-strong border-b-2 bg-raised px-1.5 font-mono text-[11.5px] font-medium leading-none text-fg",
        className,
      )}
    >
      {children}
    </kbd>
  );
}

export function Combo({ combo, className }: { combo: string; className?: string }) {
  const parts = combo.split("+").filter(Boolean);
  return (
    <span className={cx("inline-flex items-center gap-1", className)}>
      {parts.map((p, i) => (
        <KeyCap key={i}>{p}</KeyCap>
      ))}
    </span>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
}) {
  return (
    <button
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
      className={cx(
        "relative h-[22px] w-[38px] shrink-0 rounded-full border transition-colors duration-200",
        checked ? "border-accent bg-accent" : "border-line-strong bg-raised",
      )}
    >
      <span
        className={cx(
          "absolute top-[2px] size-4 rounded-full transition-[left,background-color] duration-200 ease-out",
          checked ? "left-[18px] bg-accent-ink" : "left-[2px] bg-muted",
        )}
      />
    </button>
  );
}

export function Segmented<T extends string | number>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
  label: string;
}) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-ctl border border-line bg-bg p-0.5">
      {options.map((o) => {
        const on = o.value === value;
        return (
          <button
            key={String(o.value)}
            role="radio"
            aria-checked={on}
            onClick={() => onChange(o.value)}
            className={cx(
              "h-7 rounded-[4px] px-3 font-mono text-[12.5px] transition-colors",
              on ? "bg-hover text-fg shadow-[inset_0_0_0_1px_var(--color-line-strong)]" : "text-muted hover:text-fg",
            )}
          >
            {o.label}
          </button>
        );
      })}
    </div>
  );
}

/** One settings row: label + help on the left, control on the right. */
export function Row({ title, help, children }: { title: string; help?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-8 py-3.5">
      <div className="min-w-0">
        <div className="text-[14px] text-fg">{title}</div>
        {help && <div className="mt-0.5 max-w-[56ch] text-[12.5px] leading-relaxed text-faint">{help}</div>}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

export function GameTile({ name, size = 28 }: { name: string; size?: number }) {
  const hue = gameHue(name);
  const mono = monogram(name);
  return (
    <span
      aria-hidden
      className="inline-flex shrink-0 items-center justify-center rounded-ctl font-mono font-semibold uppercase"
      style={{
        width: size,
        height: size,
        fontSize: size * 0.38,
        background: `hsl(${hue} 32% 20%)`,
        color: `hsl(${hue} 70% 78%)`,
        boxShadow: `inset 0 0 0 1px hsl(${hue} 35% 28%)`,
      }}
    >
      {mono}
    </span>
  );
}
