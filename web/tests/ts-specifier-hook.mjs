/**
 * Node cannot resolve the source tree's extensionless relative imports
 * (for example `../session` in `src/lib/api/client.ts`), and this repo's
 * node tests load those modules directly. This hook retries a failed
 * resolution with a `.ts` extension appended, which is what bundlers do.
 * Registered from the tests that need it; never active elsewhere.
 */
export async function resolve(specifier, context, next) {
	try {
		return await next(specifier, context);
	} catch (err) {
		if (specifier.startsWith('.')) {
			return next(`${specifier}.ts`, context);
		}
		throw err;
	}
}
