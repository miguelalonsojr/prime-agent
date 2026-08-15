import { type Component, truncateToWidth } from "@earendil-works/pi-tui";
import type { ReadonlyFooterDataProvider } from "../../../core/footer-data-provider.js";
import { shortenPath } from "../../../core/tools/render-utils.js";
import { theme } from "../theme/theme.js";

/**
 * Footer component for the prime brand TUI: a single dim line with the current
 * working directory (kernel cwd once known) and git branch. Other telemetry
 * (tokens, cost, model, context %) stays hidden behind /usage.
 */
export class FooterComponent implements Component {
	constructor(private footerData: ReadonlyFooterDataProvider) {}

	setAutoCompactEnabled(_enabled: boolean): void {
		// cwd/branch footer does not display compaction state
	}

	invalidate(): void {
		// state lives in FooterDataProvider; nothing cached here
	}

	dispose(): void {
		// git watcher cleanup handled by provider
	}

	render(width: number): string[] {
		const branch = this.footerData.getGitBranch();
		const path = shortenPath(this.footerData.getCwd());
		const line = branch ? `${path} (${branch})` : path;
		return [truncateToWidth(theme.fg("dim", ` ${line}`), width)];
	}
}
