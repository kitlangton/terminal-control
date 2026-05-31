import { IncompleteCaptureError, Screen, Session } from "./index.js"
import type {} from "vitest"

type MatcherResult = {
  pass: boolean
  message: () => string
}

type MatcherRegistry = {
  extend(matchers: Record<string, (received: unknown, expected: string) => Promise<MatcherResult>>): void
}

export const cellshotMatchers = {
  async toHaveScreenText(received: unknown, expected: string): Promise<MatcherResult> {
    if (!(received instanceof Session) && !(received instanceof Screen)) {
      return {
        pass: false,
        message: () => "toHaveScreenText expects a Cellshot Session or Screen",
      }
    }
    const screen = received instanceof Session ? received.screen : received
    let actual: string
    let incomplete = ""
    try {
      actual = await screen.text()
    } catch (error) {
      if (!(error instanceof IncompleteCaptureError)) throw error
      actual = error.capture.text
      incomplete = `\nCapture was incomplete: ${error.capture.reason}.`
    }
    const pass = incomplete === "" && actual === expected
    let artifact = ""
    if (!pass && received instanceof Session && received.shouldWriteFailureArtifacts()) {
      const files = await received.writeArtifacts("screen-text")
      artifact = `\nArtifacts: ${files.directory}`
    }
    return {
      pass,
      message: () => pass
        ? "expected visible terminal screen not to equal the supplied text"
        : `expected visible terminal screen:\n${expected}\n\nreceived:\n${actual}${incomplete}${artifact}`,
    }
  },
}

export function extendCellshotMatchers(expect: MatcherRegistry): void {
  expect.extend(cellshotMatchers)
}

declare module "vitest" {
  interface Assertion<T> {
    toHaveScreenText(expected: string): Promise<void>
  }

  interface AsymmetricMatchersContaining {
    toHaveScreenText(expected: string): Promise<void>
  }
}
