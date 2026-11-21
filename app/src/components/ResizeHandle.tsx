import { cx } from "./ui";

/**
 * A thin drag strip on a panel edge. `edge` is the side of the panel it sits on: dragging a
 * right-edge handle right widens the panel, dragging a left-edge handle left widens it.
 * Double-click restores the default width.
 */
export function ResizeHandle({
  edge,
  width,
  onWidth,
  onReset,
  label,
}: {
  edge: "left" | "right";
  width: number;
  onWidth: (w: number) => void;
  onReset: () => void;
  label: string;
}) {
  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={width}
      title={`${label} (double-click to reset)`}
      onDoubleClick={onReset}
      onPointerDown={(e) => {
        e.preventDefault();
        const el = e.currentTarget;
        el.setPointerCapture(e.pointerId);
        const x0 = e.clientX;
        const w0 = width;
        document.body.style.cursor = "col-resize";
        const move = (ev: PointerEvent) => onWidth(w0 + (edge === "right" ? ev.clientX - x0 : x0 - ev.clientX));
        const up = (ev: PointerEvent) => {
          move(ev);
          document.body.style.cursor = "";
          el.removeEventListener("pointermove", move);
          el.removeEventListener("pointerup", up);
        };
        el.addEventListener("pointermove", move);
        el.addEventListener("pointerup", up);
      }}
      className={cx(
        "group absolute top-0 bottom-0 z-20 w-2 cursor-col-resize touch-none",
        edge === "right" ? "-right-1" : "-left-1",
      )}
    >
      <div className="mx-auto h-full w-px bg-transparent transition-colors duration-150 group-hover:bg-accent group-active:bg-accent" />
    </div>
  );
}
