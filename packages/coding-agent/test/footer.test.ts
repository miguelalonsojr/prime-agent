import { visibleWidth } from "@earendil-works/pi-tui";
import stripAnsi from "strip-ansi";
import { beforeAll, describe, expect, it } from "vitest";
import type { ReadonlyFooterDataProvider } from "../src/core/footer-data-provider.js";
import { FooterComponent } from "../src/modes/interactive/components/footer.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";

function createFooterData(cwd: string, branch: string | null): ReadonlyFooterDataProvider {
	return {
		getCwd: () => cwd,
		getGitBranch: () => branch,
		getExtensionStatuses: () => new Map<string, string>(),
		getAvailableProviderCount: () => 1,
		onBranchChange: () => () => {},
	};
}

describe("FooterComponent content", () => {
	beforeAll(() => {
		initTheme(undefined, false);
	});

	it("renders path and branch", () => {
		const footer = new FooterComponent(createFooterData("/work/repo", "main"));
		const lines = footer.render(80);
		expect(lines).toHaveLength(1);
		expect(stripAnsi(lines[0]!)).toContain("/work/repo (main)");
	});

	it("omits the branch outside a git repo", () => {
		const footer = new FooterComponent(createFooterData("/work/plain", null));
		const lines = footer.render(80);
		expect(stripAnsi(lines[0]!)).toContain("/work/plain");
		expect(stripAnsi(lines[0]!)).not.toContain("(");
	});

	it("abbreviates the home directory", () => {
		const home = process.env.HOME ?? "";
		const footer = new FooterComponent(createFooterData(`${home}/projects/x`, null));
		expect(stripAnsi(footer.render(80)[0]!)).toContain("~/projects/x");
	});

	it("stays within narrow widths", () => {
		const footer = new FooterComponent(
			createFooterData("/a/very/long/path/that/will/not/fit/in/the/terminal", "feature/long-branch-name"),
		);
		for (const line of footer.render(24)) {
			expect(visibleWidth(line)).toBeLessThanOrEqual(24);
		}
	});
});
