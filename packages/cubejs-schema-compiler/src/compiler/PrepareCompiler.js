import { CubeValidator } from './CubeValidator';
import { DataSchemaCompiler } from './DataSchemaCompiler';
import { CubeCheckDuplicatePropTranspiler } from './CubeCheckDuplicatePropTranspiler';
import { CubePropContextTranspiler } from './CubePropContextTranspiler';
import { ImportExportTranspiler } from './ImportExportTranspiler';
import { CubeSymbols } from './CubeSymbols';
import { CubeDictionary } from './CubeDictionary';
import { CubeEvaluator } from './CubeEvaluator';
import { ContextEvaluator } from './ContextEvaluator';
import { DashboardTemplateEvaluator } from './DashboardTemplateEvaluator';
import { JoinGraph } from './JoinGraph';
import { Funnels } from '../extensions/Funnels';
import { RefreshKeys } from '../extensions/RefreshKeys';
import { Reflection } from '../extensions/Reflection';
import { CubeToMetaTransformer } from './CubeToMetaTransformer';
import { CompilerCache } from './CompilerCache';

export const prepareCompiler = (repo, options) => {
  const cubeDictionary = new CubeDictionary();
  const cubeSymbols = new CubeSymbols();
  const cubeValidator = new CubeValidator(cubeSymbols);
  const cubeEvaluator = new CubeEvaluator(cubeValidator);
  const contextEvaluator = new ContextEvaluator(cubeEvaluator);
  const joinGraph = new JoinGraph(cubeValidator, cubeEvaluator);
  const dashboardTemplateEvaluator = new DashboardTemplateEvaluator(cubeEvaluator);
  const metaTransformer = new CubeToMetaTransformer(cubeValidator, cubeEvaluator, contextEvaluator, joinGraph);
  const { maxQueryCacheSize, maxQueryCacheAge } = options;
  const compilerCache = new CompilerCache({ maxQueryCacheSize, maxQueryCacheAge });

  const transpilers = [
    new ImportExportTranspiler(),
    new CubePropContextTranspiler(cubeSymbols, cubeDictionary),
  ];

  if (!options.allowJsDuplicatePropsInSchema) {
    transpilers.push(new CubeCheckDuplicatePropTranspiler());
  }

  const compiler = new DataSchemaCompiler(repo, Object.assign({}, {
    cubeNameCompilers: [cubeDictionary],
    preTranspileCubeCompilers: [cubeSymbols, cubeValidator],
    transpilers,
    cubeCompilers: [cubeEvaluator, joinGraph, metaTransformer],
    contextCompilers: [contextEvaluator],
    dashboardTemplateCompilers: [dashboardTemplateEvaluator],
    cubeFactory: cubeSymbols.createCube.bind(cubeSymbols),
    compilerCache,
    extensions: {
      Funnels,
      RefreshKeys,
      Reflection
    },
    compileContext: options.compileContext
  }, options));
  return {
    compiler,
    metaTransformer,
    cubeEvaluator,
    contextEvaluator,
    dashboardTemplateEvaluator,
    joinGraph,
    compilerCache,
    headCommitId: options.headCommitId
  };
};

export const compile = (repo, options) => {
  const compilers = prepareCompiler(repo, options);
  return compilers.compiler.compile().then(
    () => compilers
  );
};
