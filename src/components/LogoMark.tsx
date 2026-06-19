import type { SVGProps } from "react";

interface LogoMarkProps extends SVGProps<SVGSVGElement> {
  size?: number;
}

export function LogoMark({ size = 18, ...props }: LogoMarkProps) {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 100 100"
      width={size}
      height={size}
      {...props}
    >
      <defs>
        <filter id="lm-shadow" x="-40%" y="-20%" width="180%" height="140%">
          <feDropShadow
            dx="0"
            dy="2"
            stdDeviation="3"
            floodColor="currentColor"
            floodOpacity="0.18"
          />
        </filter>
      </defs>
      <path
        d="M 50 10 A 20 20 0 0 0 50 50 A 20 20 0 0 1 50 90 L 50 10 Z"
        fill="currentColor"
        filter="url(#lm-shadow)"
      />
    </svg>
  );
}
