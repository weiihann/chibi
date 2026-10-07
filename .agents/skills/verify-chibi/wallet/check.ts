// Drives MetaMask's real NetworkController (custom-RPC network client) and the
// real balance code against an RPC URL, and logs every request on the wire.
//
//   bun run check.ts --rpc URL --address ADDR [--seconds N] [--out FILE]
//                    [--surface assets|account-tracker] [--listen-blocks] [--start-after S]
//
// --surface assets (default): @metamask/assets-controller RpcDataSource, the
//   native-balance path of MetaMask extension 13.50.0, where the remote flag
//   assetsUnifyState deprecates AccountTrackerController (extension >= 13.42).
//   Driven as AssetsController drives it for a chain the Accounts API does not
//   cover: one fetch, then subscribe (balance polling every 30 s, the
//   extension's rpcDataSourceConfig.balanceInterval).
// --surface account-tracker: @metamask/assets-controllers AccountTrackerController
//   polling (10 s), the path before assetsUnifyState.
// --listen-blocks adds a 'latest' listener to the block tracker, which starts its
//   20 s polling loop (what happens in the extension while a dapp subscription or
//   pending transaction is tracked). Without it the tracker is idle.
// --start-after S waits S seconds after the network client is up before the
//   balance code starts, as when the popup opens while the background (and its
//   block tracker) has been running for a while.
//
// Exit status: 0 when every balance read on the wire succeeded and the shown
// balance loaded, 1 otherwise.
import { Messenger, MOCK_ANY_NAMESPACE } from '@metamask/messenger';
import { NetworkController, RpcEndpointType } from '@metamask/network-controller';
import { AccountTrackerController } from '@metamask/assets-controllers';
import { RpcDataSource } from '@metamask/assets-controller';
import { appendFileSync, writeFileSync } from 'node:fs';

const args = process.argv.slice(2);
const arg = (name: string, fallback?: string): string => {
  const i = args.indexOf(`--${name}`);
  if (i !== -1 && args[i + 1] !== undefined) return args[i + 1];
  if (fallback === undefined) throw new Error(`missing --${name}`);
  return fallback;
};
const RPC = arg('rpc');
const ADDRESS = arg('address');
const SECONDS = Number(arg('seconds', '180'));
const OUT = arg('out', `wire-${Date.now()}.jsonl`);
const LISTEN_BLOCKS = args.includes('--listen-blocks');
const START_AFTER = Number(arg('start-after', '0'));
const SURFACE = arg('surface', 'assets');
if (SURFACE !== 'assets' && SURFACE !== 'account-tracker') throw new Error(`unknown --surface ${SURFACE}`);

const CHAIN_ID = '0xaa36a7';
const CAIP_CHAIN_ID = 'eip155:11155111';
const NATIVE_ASSET_ID = `${CAIP_CHAIN_ID}/slip44:60`;
const CLIENT_ID = 'custom-rpc';
const MULTICALL3 = '0xca11bde05977b3631167028862be2a173976ca11';
const t0 = Date.now();
const elapsed = () => Number(((Date.now() - t0) / 1000).toFixed(1));

type JsonRpcRequest = { id: number | string; method: string; params?: unknown[] };
type JsonRpcBody = { result?: unknown; error?: unknown };
type Wire = {
  method: string;
  params: unknown[];
  ok: boolean;
  result?: unknown;
  error?: unknown;
  headAtSend: string | { error: string };
  balanceRead: boolean;
};

writeFileSync(OUT, '');
const record = (entry: Record<string, unknown>) =>
  appendFileSync(OUT, JSON.stringify({ t: elapsed(), ...entry }) + '\n');

const realFetch = globalThis.fetch.bind(globalThis);
let probeId = 1_000_000;
const probeHead = async (): Promise<string | { error: string }> => {
  try {
    const res = await realFetch(RPC, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: probeId++, method: 'eth_blockNumber', params: [] }),
    });
    const body = (await res.json()) as JsonRpcBody;
    return typeof body.result === 'string' ? body.result : { error: JSON.stringify(body.error) };
  } catch (error) {
    return { error: String(error) };
  }
};

// A balance read is eth_getBalance, or an eth_call to Multicall3 (aggregate3 with
// getEthBalance inside, assets-controllers multicall.js getTokenBalancesForMultipleAddresses).
const isBalanceRead = (method: string, params: unknown[]): boolean => {
  if (method === 'eth_getBalance') return true;
  const call = params[0];
  return (
    method === 'eth_call' &&
    typeof call === 'object' &&
    call !== null &&
    'to' in call &&
    String(call.to).toLowerCase() === MULTICALL3
  );
};

const wire: Wire[] = [];
const logWire = (entry: Wire, extra: Record<string, unknown>) => {
  wire.push(entry);
  record({ kind: 'wire', ...extra, ...entry });
  const shown = entry.ok
    ? JSON.stringify(entry.result)?.slice(0, 90)
    : `ERROR ${typeof entry.error === 'string' ? entry.error : JSON.stringify(entry.error)}`;
  const block = ['eth_call', 'eth_getBalance'].includes(entry.method) ? ` block=${JSON.stringify(entry.params[1])}` : '';
  console.log(
    `[${elapsed()}s] ${entry.method} ${JSON.stringify(entry.params).slice(0, 110)}${block} head@send=${JSON.stringify(entry.headAtSend)} -> ${shown}`,
  );
};

// network-controller binds globalThis.fetch when it builds the RPC service
// (create-network-client.mjs createRpcServiceChain), so this wrapper sees
// exactly what MetaMask puts on the wire.
globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' || input instanceof URL ? input : input.url).href;
  if (url !== new URL(RPC).href || typeof init?.body !== 'string') return realFetch(input, init);
  const request = JSON.parse(init.body) as JsonRpcRequest;
  const params = request.params ?? [];
  const balanceRead = isBalanceRead(request.method, params);
  const headPromise = probeHead();
  const started = Date.now();
  let response: Response;
  try {
    response = await realFetch(input, init);
  } catch (error) {
    const headAtSend = await headPromise;
    logWire(
      { method: request.method, params, ok: false, error: `transport: ${String(error)}`, headAtSend, balanceRead },
      { ms: Date.now() - started },
    );
    throw error;
  }
  const text = await response.clone().text();
  const headAtSend = await headPromise;
  let body: JsonRpcBody;
  try {
    body = JSON.parse(text) as JsonRpcBody;
  } catch {
    body = { error: `non-JSON HTTP ${response.status}: ${text.slice(0, 200)}` };
  }
  logWire(
    {
      method: request.method,
      params,
      ok: body.error === undefined,
      result: body.result,
      error: body.error,
      headAtSend,
      balanceRead,
    },
    { ms: Date.now() - started, http: response.status },
  );
  return response;
}) as typeof fetch;

const account = {
  id: 'acct-1',
  address: ADDRESS.toLowerCase(),
  type: 'eip155:eoa',
  options: {},
  methods: ['personal_sign', 'eth_signTransaction'],
  scopes: ['eip155:0'],
  metadata: { name: 'Account 1', importTime: 0, keyring: { type: 'HD Key Tree' } },
};

// One namespace-free messenger stands in for the extension's root messenger; the
// controllers below are the real ones, every other controller is a state stub.
const messenger = new Messenger({ namespace: MOCK_ANY_NAMESPACE });
const stubs: Record<string, () => unknown> = {
  'ConnectivityController:getState': () => ({ connectivityStatus: 'online' }),
  'RemoteFeatureFlagController:getState': () => ({ remoteFeatureFlags: {}, cacheTimestamp: 0 }),
  'ConfigRegistryController:getState': () => ({ configs: { networks: {} } }),
  'ConfigRegistryController:getNetworkConfigByCaip2ChainId': () => undefined,
  'AnalyticsController:getState': () => ({ optedIn: false }),
  'AnalyticsController:trackEvent': () => undefined,
  'AccountsController:getSelectedAccount': () => account,
  'AccountsController:listAccounts': () => [account],
  'PreferencesController:getState': () => ({ isMultiAccountBalancesEnabled: true, useExternalServices: true }),
  'KeyringController:getState': () => ({ isUnlocked: true, keyrings: [] }),
  'NetworkEnablementController:getState': () => ({ enabledNetworkMap: { eip155: { [CHAIN_ID]: true } } }),
};
for (const [action, handler] of Object.entries(stubs)) {
  messenger.registerActionHandler(action as `${string}:${string}`, handler);
}

for (const event of [
  'NetworkController:rpcEndpointUnavailable',
  'NetworkController:rpcEndpointDegraded',
  'NetworkController:rpcEndpointChainUnavailable',
  'NetworkController:rpcEndpointChainDegraded',
  'NetworkController:rpcEndpointRetried',
] as const) {
  messenger.subscribe(event, (payload: { rpcMethodName?: string; error?: unknown }) => {
    const error = payload.error instanceof Error ? payload.error.message : payload.error && String(payload.error);
    record({ kind: 'event', event, rpcMethodName: payload.rpcMethodName, error });
    console.log(`[${elapsed()}s] event ${event} ${payload.rpcMethodName ?? ''} ${error ?? ''}`);
  });
}

type NetworkControllerOptions = ConstructorParameters<typeof NetworkController>[0];

// A user who points Sepolia at a local node: one custom endpoint on chain 0xaa36a7.
// The extension passes no getBlockTrackerOptions (metamask-extension
// app/scripts/wallet-init/instance-options/network-controller.ts), so the
// block tracker runs with network-controller's defaults.
const networkController = new NetworkController({
  // The mock messenger accepts every action and event; the typed messenger
  // parameter cannot express that.
  messenger: messenger as unknown as NetworkControllerOptions['messenger'],
  infuraProjectId: 'harness-unused',
  state: {
    selectedNetworkClientId: CLIENT_ID,
    networksMetadata: {},
    networkConfigurationsByChainId: {
      [CHAIN_ID]: {
        chainId: CHAIN_ID,
        name: 'Sepolia',
        nativeCurrency: 'SepoliaETH',
        blockExplorerUrls: [],
        defaultRpcEndpointIndex: 0,
        rpcEndpoints: [{ type: RpcEndpointType.Custom, url: RPC, networkClientId: CLIENT_ID }],
      },
    },
  },
});
networkController.init();
await networkController.lookupNetwork();
const client = networkController.getNetworkClientById(CLIENT_ID);
const metadata = networkController.state.networksMetadata[CLIENT_ID];
record({ kind: 'network', type: client.configuration.type, metadata });
console.log(`[${elapsed()}s] network client type=${client.configuration.type} metadata=${JSON.stringify(metadata)}`);

if (LISTEN_BLOCKS) {
  client.blockTracker.on('latest', (block: string) => {
    record({ kind: 'tracker', latest: block });
    console.log(`[${elapsed()}s] block tracker latest=${block} (${parseInt(block, 16)})`);
  });
}

type Poll = { t: number; balance: string | undefined; loaded: boolean };
const polls: Poll[] = [];
const notePoll = (source: string, balance: string | undefined, loaded: boolean, wireBefore: number) => {
  const poll = { t: elapsed(), balance, loaded };
  polls.push(poll);
  record({
    kind: 'poll',
    source,
    ...poll,
    balanceReads: wire
      .slice(wireBefore)
      .filter((w) => w.balanceRead)
      .map((w) => ({ method: w.method, ok: w.ok, block: w.params[1], head: w.headAtSend })),
  });
  console.log(`[${elapsed()}s] ${source}: shown balance=${balance} ${loaded ? 'LOADED' : 'NOT LOADED'}`);
};
const sleep = (ms: number) => {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
};

await sleep(START_AFTER * 1000);
let shownBalance: () => string | undefined;
if (SURFACE === 'assets') {
  type AssetsBalance = Record<string, Record<string, { amount: string }>>;
  type AssetsResponse = { assetsBalance?: AssetsBalance; assetsInfo?: Record<string, unknown> };
  const assetsState = {
    assetsBalance: {} as AssetsBalance,
    assetsInfo: {} as Record<string, unknown>,
    customAssets: {} as Record<string, string[]>,
  };
  messenger.registerActionHandler('AssetsController:getState', () => assetsState);
  // AssetsController merges data-source responses into state (updateMode 'merge').
  const merge = (response: AssetsResponse) => {
    for (const [accountId, balances] of Object.entries(response.assetsBalance ?? {})) {
      assetsState.assetsBalance[accountId] = { ...assetsState.assetsBalance[accountId], ...balances };
    }
    Object.assign(assetsState.assetsInfo, response.assetsInfo ?? {});
  };
  shownBalance = () => assetsState.assetsBalance[account.id]?.[NATIVE_ASSET_ID]?.amount;
  const loaded = () => {
    const amount = shownBalance();
    return amount !== undefined && Number(amount) > 0;
  };

  type RpcDataSourceOptions = ConstructorParameters<typeof RpcDataSource>[0];
  const rpcDataSource = new RpcDataSource({
    messenger: messenger as unknown as RpcDataSourceOptions['messenger'],
    onActiveChainsUpdated: () => undefined,
    getNativeAssetForChain: (chainId: string) => (chainId === CAIP_CHAIN_ID ? NATIVE_ASSET_ID : undefined),
    getAssetType: (assetId: string) => (assetId.includes('/slip44:') ? 'native' : 'erc20'),
    // Detection lists tokens from MetaMask's Tokens API, not the RPC; off keeps
    // the wire to balance reads.
    tokenDetectionEnabled: () => false,
    balanceInterval: 30_000,
    detectionInterval: 180_000,
    isOnboarded: () => true,
  } as RpcDataSourceOptions);
  const request = {
    accountsWithSupportedChains: [{ account, supportedChains: [CAIP_CHAIN_ID] }],
    chainIds: [CAIP_CHAIN_ID],
    dataTypes: ['balance'],
  };
  type FetchRequest = Parameters<typeof rpcDataSource.fetch>[0];
  type SubscribeRequest = Parameters<typeof rpcDataSource.subscribe>[0];

  let before = wire.length;
  const first = (await rpcDataSource.fetch(request as unknown as FetchRequest)) as AssetsResponse & { errors?: unknown };
  merge(first);
  if (first.errors) record({ kind: 'fetch-errors', errors: first.errors });
  notePoll(`RpcDataSource.fetch${first.errors ? ` errors=${JSON.stringify(first.errors)}` : ''}`, shownBalance(), loaded(), before);
  before = wire.length;
  await rpcDataSource.subscribe({
    request,
    subscriptionId: 'harness',
    isUpdate: false,
    onAssetsUpdate: async (response: AssetsResponse) => {
      merge(response);
      notePoll('RpcDataSource balance poll update', shownBalance(), loaded(), before);
      before = wire.length;
    },
  } as unknown as SubscribeRequest);
  await sleep(SECONDS * 1000);
  await rpcDataSource.unsubscribe('harness');
} else {
  type AccountTrackerOptions = ConstructorParameters<typeof AccountTrackerController>[0];
  const accountTracker = new AccountTrackerController({
    messenger: messenger as unknown as AccountTrackerOptions['messenger'],
  });
  shownBalance = () => {
    const accounts = accountTracker.state.accountsByChainId[CHAIN_ID] ?? {};
    const key = Object.keys(accounts).find((k) => k.toLowerCase() === ADDRESS.toLowerCase());
    return key === undefined ? undefined : accounts[key].balance;
  };
  // _executePoll calls this.refresh without awaiting it; wrapping the instance's
  // refresh observes each poll's outcome without changing what it does.
  const refresh = accountTracker.refresh.bind(accountTracker);
  accountTracker.refresh = async (...refreshArgs: Parameters<typeof refresh>) => {
    const before = wire.length;
    await refresh(...refreshArgs);
    const balance = shownBalance();
    notePoll('AccountTrackerController poll', balance, balance !== undefined && balance !== '0x0', before);
  };
  const pollToken = accountTracker.startPolling({ networkClientIds: [CLIENT_ID] });
  await sleep(SECONDS * 1000);
  accountTracker.stopPollingByPollingToken(pollToken);
}
await sleep(2000);
client.destroy();

const reads = wire.filter((w) => w.balanceRead);
const failed = reads.filter((w) => !w.ok);
// The block MetaMask asked for, against the server's own eth_blockNumber probed as the request left.
const notHead = (w: Wire) =>
  typeof w.params[1] === 'string' && typeof w.headAtSend === 'string' && w.params[1] !== w.headAtSend;
const byMethod: Record<string, { sent: number; failed: number }> = {};
for (const w of wire) {
  byMethod[w.method] ??= { sent: 0, failed: 0 };
  byMethod[w.method].sent += 1;
  if (!w.ok) byMethod[w.method].failed += 1;
}
const summary = {
  rpc: RPC,
  address: ADDRESS,
  seconds: SECONDS,
  surface: SURFACE,
  listenBlocks: LISTEN_BLOCKS,
  startAfter: START_AFTER,
  polls: polls.length,
  pollsLoaded: polls.filter((p) => p.loaded).length,
  finalShownBalance: shownBalance(),
  balanceReads: reads.length,
  balanceReadsFailed: failed.length,
  balanceReadsRequestedBlockNotHead: reads.filter(notHead).length,
  balanceReadsFailedRequestedBlockNotHead: failed.filter(notHead).length,
  byMethod,
  distinctErrors: [...new Set(wire.filter((w) => !w.ok).map((w) => JSON.stringify(w.error)))],
};
record({ kind: 'summary', ...summary });
console.log(JSON.stringify(summary, null, 2));
process.exit(failed.length === 0 && polls.some((p) => p.loaded) ? 0 : 1);
