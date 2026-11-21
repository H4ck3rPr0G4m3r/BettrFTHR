import { CheckCircle, Info, WarningCircle, X } from "@phosphor-icons/react";
import { useApp } from "../lib/store";
import { cx } from "./ui";

export function Toasts() {
  const toasts = useApp((s) => s.toasts);
  const dismiss = useApp((s) => s.dismiss);
  return (
    <div className="pointer-events-none fixed right-5 bottom-5 z-50 flex w-[340px] flex-col gap-2" aria-live="polite">
      {toasts.map((t) => (
        <div
          key={t.id}
          className={cx(
            "anim-rise pointer-events-auto flex items-start gap-3 rounded-card border bg-raised p-3.5 shadow-[0_16px_40px_-12px_rgb(0_0_0/0.75)]",
            t.tone === "error" ? "border-warn/30" : "border-line-strong",
          )}
        >
          {t.tone === "saved" && <CheckCircle size={19} weight="fill" className="mt-px shrink-0 text-accent" />}
          {t.tone === "info" && <Info size={19} className="mt-px shrink-0 text-muted" />}
          {t.tone === "error" && <WarningCircle size={19} weight="fill" className="mt-px shrink-0 text-warn" />}
          <div className="min-w-0 flex-1">
            <div className="text-[13.5px] font-medium">{t.title}</div>
            {t.body && <div className="mt-0.5 text-[12.5px] leading-snug break-words text-muted">{t.body}</div>}
          </div>
          {t.action && (
            <button
              onClick={() => {
                t.action!.run();
                dismiss(t.id);
              }}
              className="shrink-0 rounded-ctl px-2 py-1 text-[12.5px] font-medium text-accent hover:bg-hover"
            >
              {t.action.label}
            </button>
          )}
          <button aria-label="Dismiss" onClick={() => dismiss(t.id)} className="shrink-0 text-faint hover:text-fg">
            <X size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}
