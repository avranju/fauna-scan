import { useEffect, useState, type FormEvent } from 'react';
import { Navigate, useLocation } from 'react-router-dom';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Leaf } from 'lucide-react';
import { api, ApiError } from './api';
import { App } from './App';
import { ErrorBox, Loading } from './ui';

interface Session {
  username: string;
}

function Login({ onSignIn }: { onSignIn: (session: Session) => void }) {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    document.title = 'Sign in · Fauna Scan';
  }, []);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setSubmitting(true);
    setError('');
    try {
      const session = await api<Session>('auth/login', undefined, 'POST', {
        username,
        password,
      });
      setPassword('');
      onSignIn(session);
    } catch (error) {
      setError(
        error instanceof Error
          ? error.message
          : 'Sign-in failed. Please try again.',
      );
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <main className="login-page">
      <section className="panel login-panel" aria-labelledby="login-title">
        <span className="brand-mark">
          <Leaf size={28} />
        </span>
        <p className="eyebrow">YOUR LOCAL OBSERVATORY</p>
        <h1 id="login-title">Sign in to Fauna Scan</h1>
        <p className="muted">Your wildlife discoveries are waiting.</p>
        <form onSubmit={submit}>
          <label htmlFor="username">User name</label>
          <input
            id="username"
            name="username"
            autoComplete="username"
            autoCapitalize="none"
            spellCheck={false}
            required
            value={username}
            onChange={(event) => setUsername(event.target.value)}
            disabled={submitting}
          />
          <label htmlFor="password">Password</label>
          <input
            id="password"
            name="password"
            type="password"
            autoComplete="current-password"
            required
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            disabled={submitting}
          />
          {error && (
            <p className="login-error" role="alert">
              {error}
            </p>
          )}
          <button
            className="button primary"
            type="submit"
            disabled={submitting}
          >
            {submitting ? 'Signing in…' : 'Sign in'}
          </button>
        </form>
      </section>
    </main>
  );
}

export function Auth() {
  const client = useQueryClient();
  const location = useLocation();
  const [logoutError, setLogoutError] = useState<Error | null>(null);
  const session = useQuery<Session | null>({
    queryKey: ['auth/session'],
    queryFn: async ({ signal }) => {
      try {
        return await api<Session>('auth/session', signal);
      } catch (error) {
        if (error instanceof ApiError && error.status === 401) return null;
        throw error;
      }
    },
    retry: false,
    staleTime: 0,
    refetchInterval: 60_000,
  });

  useEffect(() => {
    const signedOut = () => {
      void client.cancelQueries();
      client.removeQueries({
        predicate: (query) => query.queryKey[0] !== 'auth/session',
      });
      client.setQueryData(['auth/session'], null);
    };

    window.addEventListener('fauna-scan:unauthenticated', signedOut);
    return () =>
      window.removeEventListener('fauna-scan:unauthenticated', signedOut);
  }, [client]);

  useEffect(() => {
    if (session.data === null) {
      void client.cancelQueries({
        predicate: (query) => query.queryKey[0] !== 'auth/session',
      });
      client.removeQueries({
        predicate: (query) => query.queryKey[0] !== 'auth/session',
      });
    }
  }, [client, session.data]);

  async function resetSession() {
    await client.cancelQueries();
    client.removeQueries({
      predicate: (query) => query.queryKey[0] !== 'auth/session',
    });
    client.setQueryData(['auth/session'], null);
  }

  async function signOut() {
    setLogoutError(null);
    try {
      await api('auth/logout', undefined, 'POST');
      await resetSession();
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        await resetSession();
      } else {
        setLogoutError(
          error instanceof Error ? error : new Error('Sign-out failed.'),
        );
      }
    }
  }

  if (session.isPending)
    return (
      <main className="login-page">
        <Loading />
      </main>
    );
  if (session.isError)
    return (
      <main className="login-page">
        <ErrorBox error={session.error} retry={() => session.refetch()} />
      </main>
    );
  if (!session.data) {
    if (location.pathname !== '/login') return <Navigate to="/login" replace />;
    return (
      <Login onSignIn={(user) => client.setQueryData(['auth/session'], user)} />
    );
  }
  if (location.pathname === '/login') return <Navigate to="/" replace />;
  return (
    <>
      {logoutError && <ErrorBox error={logoutError} retry={signOut} />}
      <App username={session.data.username} onSignOut={signOut} />
    </>
  );
}
