import { Breakpoint } from './break';
import * as wasm from './glue/pkg/glue';
import { FileSet, JsHost } from './host';
import { Labels } from './labels';
import { hex } from './util';

/** Functions the emulator may need to call. */
export interface EmulatorHost {
  exit(code: number): void;
  onWindowChanged(): void;
  showTab(name: string): void;
  onError(msg: string): void;
  onStdOut(stdout: string): void;
}

/** Wraps wasm.Emulator, able to run in a RAF loop. */
export class Emulator extends JsHost {
  readonly emu: wasm.Emulator;
  breakpoints = new Map<number, Breakpoint>();
  imports: string[] = [];
  labels: Labels;
  running = false;

  constructor(
    host: EmulatorHost,
    files: FileSet,
    readonly storageKey: string,
    bytes: Uint8Array,
    labels: Map<number, string>,
    relocate: boolean,
  ) {
    super(host, files);
    this.emu = wasm.new_emulator(this, storageKey);
    this.emu.load_exe(storageKey, bytes, relocate);

    const importsJSON = JSON.parse(this.emu.labels());
    for (const [jsAddr, jsName] of Object.entries(importsJSON)) {
      const addr = parseInt(jsAddr);
      const name = jsName as string;
      this.imports.push(`${hex(addr, 8)}: ${name}`);
      labels.set(addr, name);
    }
    this.labels = new Labels(labels);

    // // Hack: twiddle msvcrt output mode to use console.
    // this.x86.poke(0x004095a4, 1);

    this.loadBreakpoints();
  }

  private loadBreakpoints() {
    const json = window.localStorage.getItem(this.storageKey);
    if (!json) return;
    const bps = JSON.parse(json) as Breakpoint[];
    for (const bp of bps) {
      this.breakpoints.set(bp.addr, bp);
    }
  }

  private saveBreakpoints() {
    window.localStorage.setItem(this.storageKey, JSON.stringify(Array.from(this.breakpoints.values())));
  }

  addBreak(bp: Breakpoint) {
    this.breakpoints.set(bp.addr, bp);
    this.saveBreakpoints();
  }

  addBreakByName(name: string): boolean {
    for (const [addr, label] of this.labels.byAddr) {
      if (label === name) {
        this.addBreak({ addr });
        return true;
      }
    }
    if (name.match(/^[0-9a-fA-F]+$/)) {
      const addr = parseInt(name, 16);
      this.addBreak({ addr });
      return true;
    }
    return false;
  }

  delBreak(addr: number) {
    const bp = this.breakpoints.get(addr);
    if (!bp) return;
    this.breakpoints.delete(addr);
    this.saveBreakpoints();
  }

  toggleBreak(addr: number) {
    const bp = this.breakpoints.get(addr)!;
    bp.disabled = !bp.disabled;
    this.saveBreakpoints();
  }

  /** Check if the current address is a break/exit point, returning true if so. */
  isAtBreakpoint(): boolean {
    const ip = this.emu.eip;
    const bp = this.breakpoints.get(ip);
    if (bp && !bp.disabled) {
      if (bp.oneShot) {
        this.delBreak(bp.addr);
      } else {
        this.emuHost.showTab('breakpoints');
      }
      return true;
    }
    return false;
  }

  step() {
    this.emu.unblock(); // Attempt to resume any blocked threads.
    this.emu.run(1);
  }

  /** Number of instructions to execute per stepMany, adjusted dynamically. */
  stepSize = 5000;
  /** Moving average of instructions executed per millisecond. */
  instrPerMs = 0;

  private runBatch() {
    const startTime = performance.now();
    const startSteps = this.emu.instr_count;
    const cpuState = this.emu.run(this.stepSize) as wasm.CPUState;
    const endTime = performance.now();
    const endSteps = this.emu.instr_count;

    const steps = endSteps - startSteps;
    if (steps > 1000) { // only update if we ran enough instructions to get a good measurement
      const deltaTime = endTime - startTime;

      const instrPerMs = steps / deltaTime;
      const alpha = 0.5; // smoothing factor
      this.instrPerMs = alpha * (instrPerMs) + (1 - alpha) * this.instrPerMs;

      if (deltaTime < 8) {
        this.stepSize *= 2;
        console.log(`${steps} instructions in ${deltaTime.toFixed(0)}ms; adjusted step rate: ${this.stepSize}`);
      }
    }

    return cpuState;
  }

  /** Runs a batch of instructions.  Returns false if we should stop. */
  stepMany(): boolean {
    for (const bp of this.breakpoints.values()) {
      if (!bp.disabled) {
        this.emu.breakpoint_add(bp.addr);
      }
    }

    const cpuState = this.runBatch();

    for (const bp of this.breakpoints.values()) {
      if (!bp.disabled) {
        this.emu.breakpoint_clear(bp.addr);
      }
    }

    if (this.isAtBreakpoint()) {
      return false;
    }

    return cpuState == wasm.CPUState.Running;
  }

  start() {
    if (this.running) return;
    this.emu.unblock(); // Attempt to resume any blocked threads.
    // Advance past the current breakpoint, if any.
    if (this.isAtBreakpoint()) {
      this.step();
    }
    this.running = true;
    this.runFrame();
  }

  /** Runs a batch of instructions; called in RAF loop. */
  private runFrame() {
    if (!this.running) return;
    if (!this.stepMany()) {
      this.stop();
      return;
    }
    requestAnimationFrame(() => this.runFrame());
  }

  stop() {
    if (!this.running) return;
    this.running = false;
  }

  mappings(): wasm.Mapping[] {
    return JSON.parse(this.emu.mappings_json()) as wasm.Mapping[];
  }

  disassemble(addr: number): wasm.Instruction[] {
    // Note: disassemble_json() may cause allocations, invalidating any existing .memory()!
    return JSON.parse(this.emu.disassemble_json(addr, 20)) as wasm.Instruction[];
  }
}
