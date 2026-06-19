// TEMPORARY repro/diagnostic harness. Renders the wide query, auto-scrolls to
// max, and prints coverage numbers on-page so a screenshot in real WebKit
// (Safari == WKWebView) tells us whether the rightmost columns are missing from
// the DOM (range bug) or present-but-unpainted (compositor bug). Delete after.
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";

import { ResultsGrid } from "./components/ResultsGrid";
import type { CellValue, Column } from "./ipc/types";
import type { ResultSet } from "./state/editorStore";
import "./styles/global.css";

const NROWS = 5881;
const names = [
  "crmdossier_id",
  "Dossiernummer",
  "Kantoor",
  "Klant organisatie",
  "1e persoon achternaam",
  "1e persoon voorletters",
  "1e persoon tussenvoegsel",
  "2e persoon achternaam",
  "2e persoon voorletters",
  "Productnaam",
  "Maatschappij",
  "Branche",
  "Ingangsdatum",
  "Vervaldatum",
  "Premie",
  "Acceptant",
  "Adviseur",
  "Invoer schademelding",
  "Melding schade",
  "Schadedatum",
  "Schadetijd",
  "Uitkeringsdatum",
  "CRM acties",
  "Aantal openstaande taken",
  "Polisnummer",
  "rownumber",
];
const columns: Column[] = names.map((name, i) => ({
  name,
  ordinal: i,
  db_type: i === 0 || name === "rownumber" ? "int" : "nvarchar",
  logical: i === 0 || name === "rownumber" ? "integer" : "text",
  nullable: true,
}));
const rows: CellValue[][] = Array.from({ length: NROWS }, (_, r) =>
  columns.map((c): CellValue => {
    if (c.logical === "integer") return { t: "I64", v: r + 1 };
    if (c.name === "Polisnummer")
      return { t: "Decimal", v: `${7100000 + r}.021` };
    return { t: "String", v: `${c.name} ${r}` };
  }),
);
const resultSet: ResultSet = { setIndex: 0, columns, rows, affected: null };

function Harness() {
  const [info, setInfo] = useState("measuring…");
  useEffect(() => {
    const tick = () => {
      const sc = [...document.querySelectorAll("div")].find((d) => {
        const s = getComputedStyle(d);
        return (
          (s.overflowX === "auto" || s.overflow === "auto") &&
          d.scrollWidth > d.clientWidth + 20
        );
      });
      if (!sc) return;
      sc.scrollLeft = sc.scrollWidth; // jam to max
      const rect = sc.getBoundingClientRect();
      const cells = [...sc.querySelectorAll("div")].filter((d) => {
        const s = getComputedStyle(d);
        return (
          s.position === "absolute" &&
          d.getBoundingClientRect().top < rect.top + 34 &&
          d.getBoundingClientRect().width < rect.width
        );
      });
      let rightmost = 0;
      const labels: string[] = [];
      const sorted = cells
        .map((c) => ({
          r: c.getBoundingClientRect(),
          t: (c.textContent || "").trim().slice(0, 16),
        }))
        .sort((a, b) => a.r.left - b.r.left);
      for (const c of sorted)
        rightmost = Math.max(rightmost, Math.round(c.r.right - rect.left));
      for (const c of sorted.slice(-3)) labels.push(c.t);
      const cw = Math.round(sc.clientWidth);
      const gap = cw - rightmost; // px of viewport right edge not covered by any cell
      setInfo(
        `clientWidth=${cw} scrollLeft=${Math.round(sc.scrollLeft)} scrollWidth=${Math.round(sc.scrollWidth)} | ` +
          `rightmostCellEdge=${rightmost} uncoveredRightPx=${gap} | last3=[${labels.join(" | ")}]`,
      );
    };
    const id = setInterval(tick, 400);
    tick();
    return () => clearInterval(id);
  }, []);
  return (
    <>
      <div style={{ position: "fixed", inset: 0 }}>
        <ResultsGrid resultSet={resultSet} rev={1} />
      </div>
      <div
        style={{
          position: "fixed",
          top: 0,
          left: 0,
          right: 0,
          zIndex: 9999,
          background: "#b00",
          color: "#fff",
          font: "13px monospace",
          padding: "4px 8px",
          whiteSpace: "nowrap",
        }}
      >
        {info}
      </div>
    </>
  );
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <Harness />,
);
