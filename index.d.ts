/* tslint:disable */
/* eslint-disable */

/* auto-generated by NAPI-RS */

export declare class Decompressor {
  constructor()
  push(data: Buffer): { ok: true; data?: Buffer; finished: boolean } | { ok: false; error: string }
  finish(): { ok: true; data?: Buffer; finished: boolean } | { ok: false; error: string }
}
