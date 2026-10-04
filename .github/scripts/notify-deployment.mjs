import { pathToFileURL } from 'node:url';

export async function notifyDeployment({ env = process.env, fetchImpl = fetch, log = console.log } = {}) {
  const oidcUrl = new URL(env.ACTIONS_ID_TOKEN_REQUEST_URL);
  oidcUrl.searchParams.set('audience', env.DEPLOY_OIDC_AUDIENCE);
  const tokenResponse = await fetchImpl(oidcUrl, {
    headers: { Authorization: `Bearer ${env.ACTIONS_ID_TOKEN_REQUEST_TOKEN}` },
    redirect: 'error',
    signal: AbortSignal.timeout(10_000),
  });
  log(`OIDC token request returned HTTP ${tokenResponse.status}.`);
  if (!tokenResponse.ok) throw new Error('OIDC token request failed');

  const { value: token } = await tokenResponse.json();
  log(`::add-mask::${token}`);
  const response = await fetchImpl(env.DEPLOY_WEBHOOK_URL, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ service: env.DEPLOY_SERVICE_ID }),
    redirect: 'error',
    signal: AbortSignal.timeout(10_000),
  });
  log(`Deployment webhook returned HTTP ${response.status}.`);
  if (!response.ok) throw new Error('Deployment webhook rejected the request');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    await notifyDeployment();
  } catch {
    // Fetch errors and response bodies may contain credentials; keep the warning generic.
    console.log('::warning::Deployment notification failed. The image is already published.');
    process.exitCode = 1;
  }
}
