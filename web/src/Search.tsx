import { useEffect, useRef, useState } from 'react';
import { useLocation, useNavigate, useSearchParams } from 'react-router-dom';
import { Search as SearchIcon, X } from 'lucide-react';
import { searchParams } from './filters';

export function Search() {
  const [params] = useSearchParams();
  const location = useLocation();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [term, setTerm] = useState(params.get('q') || '');
  const container = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    setTerm(params.get('q') || '');
    setOpen(false);
  }, [location.pathname, location.search]);

  useEffect(() => {
    if (!open) return;
    input.current?.focus();
    input.current?.select();

    function outside(event: PointerEvent) {
      if (!container.current?.contains(event.target as Node)) setOpen(false);
    }

    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [open]);

  function close() {
    setOpen(false);
    trigger.current?.focus();
  }

  return (
    <div ref={container} className="header-search">
      <button
        ref={trigger}
        className="search-toggle"
        aria-label="Search images"
        aria-expanded={open}
        aria-controls="archive-search"
        title="Search images"
        onClick={() => setOpen(!open)}
      >
        <SearchIcon size={19} />
      </button>
      {open && (
        <form
          id="archive-search"
          className="search-form panel"
          role="search"
          aria-label="Search the archive"
          onKeyDown={(event) => {
            if (event.key === 'Escape') {
              event.preventDefault();
              close();
            }
          }}
          onSubmit={(event) => {
            event.preventDefault();
            if (!term.trim()) return;
            navigate(`/images?${searchParams(term, params)}`);
            close();
          }}
        >
          <div className="row">
            <label htmlFor="archive-search-term">Search the archive</label>
            <button type="button" aria-label="Close search" onClick={close}>
              <X size={16} />
            </button>
          </div>
          <div className="search-entry">
            <input
              ref={input}
              id="archive-search-term"
              type="search"
              value={term}
              onChange={(event) => setTerm(event.target.value)}
              placeholder="Try boar, deer, or garden…"
              aria-describedby="archive-search-help"
              maxLength={200}
              required
            />
            <button type="submit" className="primary" disabled={!term.trim()}>
              Search
            </button>
          </div>
          <p id="archive-search-help" className="muted">
            Species, model descriptions, and cameras. All dates and cameras.
          </p>
        </form>
      )}
    </div>
  );
}
