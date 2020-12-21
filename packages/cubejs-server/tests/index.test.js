/* globals describe,test,expect,jest,afterEach */
/* eslint-disable no-underscore-dangle */

import http from 'http';
import { CubejsServer as CubeServer } from '../src/server';

jest.mock('@cubejs-backend/server-core', () => {
  const staticCreate = jest.fn();
  const initApp = jest.fn(() => Promise.resolve());
  const event = jest.fn(() => Promise.resolve());
  const releaseConnections = jest.fn(() => Promise.resolve());

  class CubejsServerCore {
    static create() {
      // eslint-disable-next-line prefer-rest-params
      staticCreate.call(null, arguments);
      return new CubejsServerCore();
    }

    initApp() {
      return initApp();
    }

    event() {
      return event();
    }

    releaseConnections() {
      return releaseConnections();
    }
  }
  CubejsServerCore.mock = {
    staticCreate,
    initApp,
    event,
    releaseConnections,
  };
  return CubejsServerCore;
});

// eslint-disable-next-line global-require
jest.mock('http', () => require('./__mocks__/http'));

describe('CubeServer', () => {
  describe('listen', () => {
    afterEach(() => jest.clearAllMocks());

    test(
      'should create an http server that listens to the PORT',
      async () => {
      // arrange
        const server = new CubeServer();
        // act
        await server.listen();
        // assert
        expect(http.createServer).toHaveBeenCalledTimes(1);
        expect(http.__mockServer.listen).toHaveBeenCalledTimes(1);
        expect(http.__mockServer.listen.mock.calls[0][0]).toBe(4000);
      }
    );

    test('given an option object, should pass the options object to the created server instance', async () => {
      // arrange
      const options = {};
      const server = new CubeServer();
      // act
      await server.listen(options);
      // assert
      expect(http.createServer.mock.calls[0][0]).toBe(options);
    });

    test('given a successful server listen, should resolve the app, the port(s) and the server instance', async () => {
      // arrange
      const cubeServer = new CubeServer();

      {
        // act
        const { app, port, server } = await cubeServer.listen();
        // assert
        expect(app).toBeInstanceOf(Function);
        expect(port).toBe(4000);
        expect(server).toBe(http.__mockServer);
      }
    });

    test(
      'given a failed server listen, should reject the error and reset the server',
      async () => {
        // arrange
        const error = new Error('I\'m a Teapot');
        http.__mockServer.listen.mockImplementationOnce(
          (opts, cb) => cb && cb(error)
        );

        const cubeServer = new CubeServer();
        // act
        try {
          await cubeServer.listen();
        } catch (err) {
        // assert
          expect(err).toBe(error);
          expect(cubeServer.server).toBe(null);
        }
      }
    );

    test('should not be able to listen if the server is already listening', async () => {
      // arrange
      const cubeServer = new CubeServer();
      // act
      try {
        await cubeServer.listen();
        await cubeServer.listen();
      } catch (err) {
      // assert
        expect(err.message).toBe('CubeServer is already listening');
        expect(http.createServer).toHaveBeenCalledTimes(1);
        expect(http.__mockServer.listen).toHaveBeenCalledTimes(1);
        expect(cubeServer.server).not.toBe(null);
      }
    });
  });

  describe('close', () => {
    test('should not be able to close the server if the server isn\'t already listening', async () => {
      const cubeServer = new CubeServer();
      // act
      try {
        await cubeServer.listen();
        await cubeServer.close();
        await cubeServer.close();
      } catch (err) {
      // assert
        expect(err.message).toBe('CubeServer is not started.');
        expect(cubeServer.server).toBe(null);
      }
    });
  });
});
