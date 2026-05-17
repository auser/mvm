/**
 * Runtime-side secret substitution helpers.
 *
 * Cloud SDK adapters resolve placeholders at credential-load time so
 * AWS SigV4/GCP JWT/Azure SAS signing sees the real credential before
 * computing request signatures. The vsock transport lands with W3; this
 * registry is the stable SDK contract the transport plugs into.
 */

export type SubstitutionHandler = (placeholder: string) => string | Promise<string>;

const handlers = new Map<string, SubstitutionHandler>();

export class SubstitutionHandlerError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SubstitutionHandlerError";
  }
}

export interface AwsCredentials {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
}

export function register_substitution_handler(
  name: string,
  fn: SubstitutionHandler,
): void {
  if (name.length === 0) {
    throw new Error("substitution handler name must be non-empty");
  }
  handlers.set(name, fn);
}

export function clear_substitution_handlers(): void {
  handlers.clear();
}

export async function substitute(name: string, placeholder: string): Promise<string> {
  const handler = handlers.get(name);
  if (handler === undefined) {
    throw new SubstitutionHandlerError(
      `no substitution handler registered for ${JSON.stringify(name)}`,
    );
  }
  return await handler(placeholder);
}

export async function aws_credentials_from_placeholders(input: {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
}): Promise<AwsCredentials> {
  const creds: AwsCredentials = {
    accessKeyId: await substitute("aws", input.accessKeyId),
    secretAccessKey: await substitute("aws", input.secretAccessKey),
  };
  if (input.sessionToken !== undefined) {
    creds.sessionToken = await substitute("aws", input.sessionToken);
  }
  return creds;
}

export function is_placeholder(value: string): boolean {
  return value.startsWith("mvm-secret://");
}
