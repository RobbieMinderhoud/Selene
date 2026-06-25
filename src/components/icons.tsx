/**
 * Central icon set for Selene.
 *
 * The rest of the app imports semantic names from here and never references
 * `lucide-react` directly, so re-theming a glyph — or making the connection
 * icon driver-aware once pg/mysql/sqlite land (v0.3) — is a one-line change in
 * this file.
 *
 * Defaults: 14px at stroke-width 1.75 reads crisp and light at our small UI
 * sizes (lucide's own defaults are 24 / 2, too heavy here). Icons are purely
 * decorative — the buttons and rows that hold them already carry the
 * `aria-label`/`title` — so they default to `aria-hidden`. Every prop stays
 * overridable per use (size, strokeWidth, color, className…); colour follows
 * `currentColor`, so icons inherit the active theme with no per-theme work.
 */

import {
  Server,
  Database,
  Folder,
  FileCode,
  Table2,
  Eye,
  KeyRound,
  Circle,
  Plus,
  Pencil,
  Trash2,
  X,
  Unplug,
  RotateCw,
  ChevronRight,
  ChevronDown,
  Play,
  Square,
  DatabaseZap,
  Download,
  Copy,
  CircleAlert,
  CircleCheck,
  Info,
  Check,
  GripVertical,
  GripHorizontal,
  Search,
  Replace,
  ReplaceAll,
  CaseSensitive,
  Regex,
  WholeWord,
  ChevronUp,
  type LucideIcon,
  type LucideProps,
} from "lucide-react";

/** Wrap a lucide glyph with our shared defaults; `defaults` then `props` win. */
function icon(Glyph: LucideIcon, defaults?: Partial<LucideProps>) {
  function Icon(props: LucideProps) {
    return (
      <Glyph
        size={14}
        strokeWidth={1.75}
        aria-hidden={true}
        {...defaults}
        {...props}
      />
    );
  }
  Icon.displayName = `Icon(${Glyph.displayName ?? "lucide"})`;
  return Icon;
}

// Schema / connection hierarchy: Server → Database → Schema → Table → Column.
export const ConnectionIcon = icon(Server); // saved connection / server instance
export const DatabaseIcon = icon(Database);
export const SchemaIcon = icon(Folder);
export const TableIcon = icon(Table2);
export const ViewIcon = icon(Eye);
export const PrimaryKeyIcon = icon(KeyRound);
export const ColumnIcon = icon(Circle, { size: 9 }); // small neutral field bullet

// Workspace file tree.
export const FolderIcon = icon(Folder);
export const FileIcon = icon(FileCode); // a `.sql` file

// Actions / affordances.
export const AddIcon = icon(Plus, { size: 15 });
export const EditIcon = icon(Pencil);
export const DeleteIcon = icon(Trash2);
export const CloseIcon = icon(X);
export const DisconnectIcon = icon(Unplug);
export const ReconnectIcon = icon(RotateCw); // re-open a dropped connection
export const CaretIcon = icon(ChevronRight); // rotates 90° on expand (CSS)
export const DropdownIcon = icon(ChevronDown);
export const RunIcon = icon(Play);
export const CancelIcon = icon(Square);
export const CheckIcon = icon(Check);
export const MultiTargetIcon = icon(DatabaseZap); // run on multiple targets
export const DownloadIcon = icon(Download);
export const CopyIcon = icon(Copy);
export const DragHandleIcon = icon(GripVertical, { size: 12 });
export const PanelGripIcon = icon(GripHorizontal, { size: 12 });

// Editor find/replace overlay.
export const SearchIcon = icon(Search);
export const ReplaceIcon = icon(Replace);
export const ReplaceAllIcon = icon(ReplaceAll);
export const MatchCaseIcon = icon(CaseSensitive, { size: 16 }); // "Aa" glyph reads better a touch larger
export const RegexIcon = icon(Regex, { size: 16 }); // ".*" glyph
export const WholeWordIcon = icon(WholeWord, { size: 16 }); // "ab|" glyph
export const PrevMatchIcon = icon(ChevronUp);
export const NextMatchIcon = icon(ChevronDown);

// Toast severity.
export const ErrorIcon = icon(CircleAlert, { size: 16 });
export const SuccessIcon = icon(CircleCheck, { size: 16 });
export const InfoIcon = icon(Info, { size: 16 });
