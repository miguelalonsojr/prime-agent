import { type Component, truncateToWidth } from "@earendil-works/pi-tui";
import type { ReadonlyFooterDataProvider } from "../../../core/footer-data-provider.js";
import { shortenPath } from "../../../core/tools/render-utils.js";
import { theme } from "../theme/theme.js";

export class FooterComponent implements Component {
	constructor(private footerData: ReadonlyFooterDataProvider) {}

	setAutoCompactEnabled(_enabled: boolean): void {}

	invalidate(): void {}

	dispose(): void {}

	render(width: number): string[] {
		const branch = this.footerData.getGitBranch();
		const path = shortenPath(this.footerData.getCwd());
		const line = branch ? `${path} (${branch})` : path;
		return [truncateToWidth(theme.fg("dim", ` ${line}`), width)];
	}
}
