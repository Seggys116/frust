#include "llvm/Bitcode/BitcodeWriter.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/PassManager.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Plugins/PassPlugin.h"
#include "llvm/Support/ErrorHandling.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/Transforms/IPO/StripDeadPrototypes.h"
#include "llvm/Transforms/Utils/SplitModule.h"

#include <cstdlib>

namespace {

void sortDeclarations(llvm::Module &module) {
  // Preserve the original order of definitions.
  module.getFunctionList().sort([](const auto &left, const auto &right) {
    if (left.isDeclaration() != right.isDeclaration())
      return !left.isDeclaration();
    return left.isDeclaration() && left.getName() < right.getName();
  });
}

struct SortDeclarations : llvm::PassInfoMixin<SortDeclarations> {
  llvm::PreservedAnalyses run(llvm::Module &module,
                             llvm::ModuleAnalysisManager &) {
    sortDeclarations(module);
    return llvm::PreservedAnalyses::none();
  }
};

struct SplitCanonicalPartitions : llvm::PassInfoMixin<SplitCanonicalPartitions> {
  llvm::PreservedAnalyses run(llvm::Module &module,
                             llvm::ModuleAnalysisManager &) {
    const char *prefix = std::getenv("FRUST_PARTITION_PREFIX");
    const char *count = std::getenv("FRUST_PARTITION_COUNT");
    unsigned partitions = 0;
    if (!prefix || !*prefix || !count ||
        llvm::StringRef(count).getAsInteger(10, partitions) || partitions == 0)
      llvm::report_fatal_error("missing or invalid partition output settings");

    unsigned index = 0;
    llvm::SplitModule(module, partitions,
                      [&](std::unique_ptr<llvm::Module> partition) {
      llvm::ModuleAnalysisManager analyses;
      llvm::StripDeadPrototypesPass().run(*partition, analyses);
      sortDeclarations(*partition);
      const std::string path = (llvm::Twine(prefix) + llvm::Twine(index++)).str();
      std::error_code error;
      llvm::raw_fd_ostream output(path, error, llvm::sys::fs::OF_None);
      if (error)
        llvm::report_fatal_error(llvm::Twine("open partition output: ") + error.message());
      llvm::WriteBitcodeToFile(*partition, output);
      output.close();
      if (output.has_error())
        llvm::report_fatal_error("could not write canonical partition");
    });
    return llvm::PreservedAnalyses::none();
  }
};

} // namespace

extern "C" llvm::PassPluginLibraryInfo llvmGetPassPluginInfo() {
  return {LLVM_PLUGIN_API_VERSION, "frust-sort-declarations", LLVM_VERSION_STRING,
          [](llvm::PassBuilder &builder) {
            builder.registerPipelineParsingCallback(
                [](llvm::StringRef name, llvm::ModulePassManager &manager,
                   llvm::ArrayRef<llvm::PassBuilder::PipelineElement>) {
                  if (name == "frust-sort-declarations")
                    manager.addPass(SortDeclarations());
                  else if (name == "frust-split-canonical-partitions")
                    manager.addPass(SplitCanonicalPartitions());
                  else
                    return false;
                  return true;
                });
          }};
}
