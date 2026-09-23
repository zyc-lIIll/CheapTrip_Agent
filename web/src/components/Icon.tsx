import type { SVGProps } from "react";

export type IconName =
  | "add"
  | "close"
  | "delete"
  | "edit"
  | "menu"
  | "memory"
  | "moon"
  | "send"
  | "stop"
  | "sun";

const paths: Record<IconName, string> = {
  add: "M12 5v14M5 12h14",
  close: "m6 6 12 12M18 6 6 18",
  delete: "M4 7h16M9 7V4h6v3m3 0-1 13H7L6 7m4 4v5m4-5v5",
  edit: "m4 20 4.2-1 10.9-10.9a2.1 2.1 0 0 0-3-3L5.2 16 4 20Zm10.5-13.5 3 3",
  menu: "M4 7h16M4 12h16M4 17h16",
  memory: "M9 3h6v3h3v12h-3v3H9v-3H6V6h3V3Zm0 6h6m-6 4h6",
  moon: "M20 15.2A8 8 0 0 1 8.8 4 8 8 0 1 0 20 15.2Z",
  send: "m4 4 17 8-17 8 3-8-3-8Zm3 8h14",
  stop: "M7 7h10v10H7z",
  sun: "M12 3v2m0 14v2M3 12h2m14 0h2M5.6 5.6 7 7m10 10 1.4 1.4m0-12.8L17 7M7 17l-1.4 1.4M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0Z",
};

export function Icon({ name, ...props }: { name: IconName } & SVGProps<SVGSVGElement>) {
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      {...props}
    >
      <path d={paths[name]} />
    </svg>
  );
}
