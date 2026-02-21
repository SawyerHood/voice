import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  formatDuration,
  formatHistoryTimestamp,
  formatLanguageCode,
  formatProvider,
  type HistoryEntry,
} from "./historyUtils";

const HISTORY_PAGE_SIZE = 25;

type EntryAction = "copy" | "insert" | "delete";
type ActiveEntryAction = { id: string; type: EntryAction } | null;

function toErrorMessage(error: unknown, fallbackMessage: string): string {
  if (typeof error === "string" && error.trim()) {
    return error;
  }

  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }

  return fallbackMessage;
}

type HistoryPanelProps = {
  refreshSignal?: number;
};

function HistoryPanel({ refreshSignal = 0 }: HistoryPanelProps) {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [offset, setOffset] = useState(0);
  const [hasMore, setHasMore] = useState(true);
  const [isLoading, setIsLoading] = useState(false);
  const [isClearingAll, setIsClearingAll] = useState(false);
  const [activeAction, setActiveAction] = useState<ActiveEntryAction>(null);
  const [loadError, setLoadError] = useState("");
  const [actionError, setActionError] = useState("");
  const [actionNotice, setActionNotice] = useState("");
  const previousRefreshSignal = useRef(refreshSignal);

  const loadEntries = useCallback(async (nextOffset: number, replace: boolean) => {
    setIsLoading(true);
    setLoadError("");

    try {
      const page = await invoke<HistoryEntry[]>("list_history", {
        limit: HISTORY_PAGE_SIZE,
        offset: nextOffset,
      });

      setEntries((existingEntries) => {
        if (replace) {
          return page;
        }

        const existingIds = new Set(existingEntries.map((entry) => entry.id));
        const uniquePageEntries = page.filter((entry) => !existingIds.has(entry.id));
        return [...existingEntries, ...uniquePageEntries];
      });
      setOffset(nextOffset + page.length);
      setHasMore(page.length === HISTORY_PAGE_SIZE);
    } catch (error) {
      setLoadError(toErrorMessage(error, "Failed to load transcript history."));
    } finally {
      setIsLoading(false);
    }
  }, []);

  const refreshHistory = useCallback(async () => {
    await loadEntries(0, true);
  }, [loadEntries]);

  useEffect(() => {
    void refreshHistory();
  }, [refreshHistory]);

  useEffect(() => {
    if (previousRefreshSignal.current === refreshSignal) {
      return;
    }

    previousRefreshSignal.current = refreshSignal;
    void refreshHistory();
  }, [refreshHistory, refreshSignal]);

  const runEntryAction = useCallback(
    async (
      entryId: string,
      actionType: EntryAction,
      work: () => Promise<void>,
      successMessage: string,
    ) => {
      setActionError("");
      setActionNotice("");
      setActiveAction({ id: entryId, type: actionType });

      try {
        await work();
        setActionNotice(successMessage);
      } catch (error) {
        setActionError(toErrorMessage(error, "Unable to complete history action."));
      } finally {
        setActiveAction(null);
      }
    },
    [],
  );

  const onCopy = useCallback(
    (entry: HistoryEntry) => {
      void runEntryAction(
        entry.id,
        "copy",
        () => invoke("copy_to_clipboard", { text: entry.text }),
        "Transcript copied to clipboard.",
      );
    },
    [runEntryAction],
  );

  const onReinsert = useCallback(
    (entry: HistoryEntry) => {
      void runEntryAction(
        entry.id,
        "insert",
        () => invoke("insert_text", { text: entry.text }),
        "Transcript re-inserted into the focused app.",
      );
    },
    [runEntryAction],
  );

  const onDelete = useCallback(
    (entry: HistoryEntry) => {
      if (!window.confirm("Delete this transcript entry?")) {
        return;
      }

      void runEntryAction(
        entry.id,
        "delete",
        async () => {
          const deleted = await invoke<boolean>("delete_history_entry", { id: entry.id });
          if (!deleted) {
            throw new Error("That entry was already deleted.");
          }
          await refreshHistory();
        },
        "Transcript deleted.",
      );
    },
    [refreshHistory, runEntryAction],
  );

  const onLoadMore = useCallback(() => {
    if (isLoading || !hasMore) {
      return;
    }

    void loadEntries(offset, false);
  }, [hasMore, isLoading, loadEntries, offset]);

  const onClearAll = useCallback(() => {
    if (!entries.length || isClearingAll) {
      return;
    }

    if (!window.confirm("Clear all transcript history? This cannot be undone.")) {
      return;
    }

    void (async () => {
      setIsClearingAll(true);
      setActionError("");
      setActionNotice("");

      try {
        await invoke("clear_history");
        setEntries([]);
        setOffset(0);
        setHasMore(false);
        setActionNotice("History cleared.");
      } catch (error) {
        setActionError(toErrorMessage(error, "Failed to clear transcript history."));
      } finally {
        setIsClearingAll(false);
      }
    })();
  }, [entries.length, isClearingAll]);

  return (
    <section className="history-panel">
      <div className="history-toolbar">
        <p className="history-title">Recent transcripts</p>
        <div className="history-toolbar-actions">
          <button
            className="utility-button"
            type="button"
            onClick={() => void refreshHistory()}
            disabled={isLoading || isClearingAll}
          >
            Refresh
          </button>
          <button
            className="utility-button destructive"
            type="button"
            onClick={onClearAll}
            disabled={!entries.length || isLoading || isClearingAll}
          >
            {isClearingAll ? "Clearing..." : "Clear All"}
          </button>
        </div>
      </div>

      {loadError ? <p className="history-error">{loadError}</p> : null}
      {actionError ? <p className="history-error">{actionError}</p> : null}
      {actionNotice ? <p className="history-notice">{actionNotice}</p> : null}

      <div className="history-list" role="list">
        {!isLoading && entries.length === 0 ? (
          <div className="history-empty">
            <div className="history-empty-icon" aria-hidden="true">📝</div>
            <p className="history-empty-title">No transcripts yet</p>
            <p className="history-empty-description">
              Press your hotkey to record — transcripts will appear here.
            </p>
          </div>
        ) : null}

        {entries.map((entry) => {
          const entryActionActive = activeAction?.id === entry.id;
          const entryActionsDisabled = entryActionActive || isClearingAll;

          return (
            <article className="history-entry" key={entry.id} role="listitem">
              <p className="history-text" title={entry.text}>
                {entry.text}
              </p>

              <p className="history-meta">
                <span className="history-chip">{formatHistoryTimestamp(entry.timestamp)}</span>
                <span className="history-chip">{formatDuration(entry.durationSecs)}</span>
                <span className="history-chip">{formatLanguageCode(entry.language)}</span>
                <span className="history-chip history-provider-chip">
                  {formatProvider(entry.provider)}
                </span>
              </p>

              <div className="history-entry-actions">
                <button
                  className="entry-action"
                  type="button"
                  onClick={() => onCopy(entry)}
                  disabled={entryActionsDisabled}
                >
                  {entryActionActive && activeAction?.type === "copy" ? "Copying..." : "Copy"}
                </button>
                <button
                  className="entry-action"
                  type="button"
                  onClick={() => onReinsert(entry)}
                  disabled={entryActionsDisabled}
                >
                  {entryActionActive && activeAction?.type === "insert"
                    ? "Re-inserting..."
                    : "Re-insert"}
                </button>
                <button
                  className="entry-action danger"
                  type="button"
                  onClick={() => onDelete(entry)}
                  disabled={entryActionsDisabled}
                >
                  {entryActionActive && activeAction?.type === "delete" ? "Deleting..." : "Delete"}
                </button>
              </div>
            </article>
          );
        })}
      </div>

      {isLoading ? <p className="history-loading">Loading history...</p> : null}

      {!isLoading && hasMore && entries.length > 0 ? (
        <button className="history-load-more" type="button" onClick={onLoadMore}>
          Load More
        </button>
      ) : null}
    </section>
  );
}

export default HistoryPanel;
